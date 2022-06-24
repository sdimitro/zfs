use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;

use anyhow::Result;
use log::*;
use lru::LruCache;

use crate::measure;
use crate::super_trace;
use crate::watch_once;

pub struct AsyncCache<K: Hash + Eq + Copy + Debug, V: Clone> {
    inner: std::sync::Mutex<Inner<K, V>>,
}

struct Inner<K: Hash + Eq + Copy + Debug, V: Clone> {
    cache: LruCache<K, V>,
    loading: HashMap<K, watch_once::Receiver<V>>,
}

pub enum GetMethod {
    Cached,
    WaitedForInProgressLoad,
    Loaded,
}

impl<K: Hash + Eq + Copy + Debug, V: Clone> AsyncCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner {
                cache: LruCache::new(capacity),
                loading: Default::default(),
            }),
        }
    }

    /// Same as get_method() but doesn't return the GetMethod
    pub async fn get<F, T>(&self, key: K, loader: T) -> Result<V>
    where
        F: Future<Output = Result<V>>,
        T: FnOnce(K) -> F,
    {
        self.get_method(key, loader).await.map(|(value, _)| value)
    }

    /// If the value is already in the cache, return it.  If it is already being loaded by a
    /// concurrent get(), wait for that to complete and then return the value loaded by the first
    /// get(), assuming it is not invalidated or fails.  Otherwise, call `loader().await` to
    /// retrieve the value, and add it to the cache unless this load was invalidated.  `Err`
    /// returns from `loader().await` are not cached.
    pub async fn get_method<F, T>(&self, key: K, loader: T) -> Result<(V, GetMethod)>
    where
        F: Future<Output = Result<V>>,
        T: FnOnce(K) -> F,
    {
        let mut method = None;
        let tx = loop {
            let rx = {
                let mut inner = self.inner.lock().unwrap();
                if let Some(value) = inner.cache.get(&key) {
                    // Found cached value
                    measure!("AsyncCache hit").hit();
                    return Ok((value.clone(), method.unwrap_or(GetMethod::Cached)));
                }
                match inner.loading.get(&key) {
                    None => {
                        // No cached value, loading not in progress: drop lock and initiate load
                        let (tx, rx) = watch_once::channel();
                        inner.loading.insert(key, rx);
                        break tx;
                    }
                    // loading in progress: drop lock and wait for load
                    Some(rx) => rx.clone(),
                }
            };

            // Loading in progress: wait for it
            super_trace!("{:?}: found GET in progress, waiting", key);
            match measure!("AsyncCache wait for in-progress load")
                .fut(rx.recv())
                .await
            {
                Ok(value) => {
                    // In-progress load was Ok(), and was not invalidated
                    return Ok((value, GetMethod::WaitedForInProgressLoad));
                }
                Err(_) => {
                    // Sender dropped because it doesn't have a value for us, due to the loader()
                    // returning an Err, or due to invalidate().  loop around and retry.
                    measure!("AsyncCache retrying failed/invalidated load").hit();
                    debug!("{:?}: waited for failed/invalidated GET, retrying", key);
                    method = Some(GetMethod::WaitedForInProgressLoad);
                }
            }
        };
        // No cached value, load not in progress: initiate load
        let result = measure!("AsyncCache loading").fut(loader(key)).await;
        // We need to remove the `loading` entry regardless of the result
        let mut inner = self.inner.lock().unwrap();

        let cacheable = match inner.loading.entry(key) {
            Entry::Occupied(oe) => {
                let cacheable = oe.get().same_channel(&tx.subscribe());
                if cacheable {
                    oe.remove();
                }
                cacheable
            }
            Entry::Vacant(_) => false,
        };

        super_trace!(
            "load completed for {:?}; Ok={}, cacheable={}",
            key,
            result.is_ok(),
            cacheable
        );
        result.map(|value| {
            // This load may have been marked non-cacheable by invalidate().  In that case, the
            // object's contents have been changed by a concurrent PUT, but since we initiated
            // our GET before the PUT, the old value is sufficient for us, and any other get()'s
            // that were initiated before the PUT (and thus subscribed to our `tx`).  But we
            // don't want other GET's (which may have been initiated after the PUT completed) to
            // see the potentially-old value that we got, so we don't add it to the cache.
            if cacheable {
                inner.cache.put(key, value.clone());
            } else {
                measure!("AsyncCache load invalidated").hit();
            }
            tx.send(value.clone()).ok();
            (value, GetMethod::Loaded)
        })
    }

    /// If the value is already in the cache, return it.  If it is being loaded by a concurrent
    /// get(), wait for that to complete and then return the value loaded by the other get().
    /// Otherwise, return None.
    pub async fn get_without_loading(&self, key: K) -> Option<V> {
        // need this block separate so that we can drop the mutex before the .await
        let rx = {
            let mut inner = self.inner.lock().unwrap();
            match inner.cache.get(&key) {
                Some(value) => return Some(value.clone()),
                None => inner.loading.get(&key).cloned(),
            }
        };
        match rx {
            None => None,
            Some(rx) => rx.recv().await.ok(),
        }
    }

    /// Invalidate the value for this key.  If there is a concurrent get(), its value is also
    /// invalidated and will not be cached (though it can still be returned to any get()'s that
    /// were initiated before this call to invalidate().
    pub fn invalidate(&self, key: K) {
        let mut inner = self.inner.lock().unwrap();
        inner.cache.pop(&key);
        // If there's a concurrent get() -> loader() going on, it may see the old value, which is
        // fine.  But we can't allow new get()s to see the old value, either via the watch
        // channel or by finding it in the cache later.  We remove the `loading` entry, so that
        // subsequent get()'s can not see the value that the in-progress get() retrieves.  The
        // in-progress get() will notice that its entry was removed, and not add its value to the
        // cache.  Since we need a way to communicate with the in-progress get(), the state in
        // `loading` can't be part of `cache`, because it could be evicted.
        if inner.loading.remove(&key).is_some() {
            measure!("AsyncCache invalidated load").hit();
            trace!("invalidated in-progress loader() of {:?}", key);
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use futures::future;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;

    type Key = u64;
    type Value = i32;

    struct Sender {
        tx: watch_once::Sender<Value>,
        join_handle: JoinHandle<Value>,
    }
    impl Sender {
        async fn send(self, value: Value) -> Value {
            self.tx.send(value).unwrap();
            self.join_handle.await.unwrap()
        }
    }

    // returns (future to await load starting, Sender)
    async fn getter(cache: Arc<AsyncCache<Key, Value>>, key: Key) -> (impl Future, Sender) {
        let (tx, rx) = watch_once::channel::<Value>();
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let join_handle = tokio::spawn(async move {
            cache
                .get(key, move |_| {
                    started_tx.send(()).ok();
                    async move { Ok(rx.recv().await.unwrap()) }
                })
                .await
                .unwrap()
        });

        (started_rx, Sender { tx, join_handle })
    }

    #[tokio::test]
    async fn double_get() {
        let cache = Arc::new(AsyncCache::new(10));
        let key = 123;

        // kick off first get
        let (started1, tx1) = getter(cache.clone(), key).await;
        started1.await;

        // kick off second get, it should wait for first get to complete, and use the cached value
        let fut2 = cache.get(key, |_| future::ready(Ok(2)));

        // complete first get, which should be added to the cache
        assert_eq!(tx1.send(1).await, 1);

        // complete second get, it should see the cached value
        assert_eq!(fut2.await.unwrap(), 1);

        // more gets also see cached value
        assert_eq!(cache.get(key, |_| future::ready(Ok(3))).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrent_gets_fifo() {
        let cache = Arc::new(AsyncCache::new(10));

        let (started1, tx1) = getter(cache.clone(), 100).await;
        started1.await;

        let (started2, tx2) = getter(cache.clone(), 200).await;
        started2.await;

        // both loader()'s are now in progress.  Complete them in FIFO.

        assert_eq!(tx1.send(1).await, 1);
        assert_eq!(tx2.send(2).await, 2);
    }

    #[tokio::test]
    async fn concurrent_gets_lifo() {
        let cache = Arc::new(AsyncCache::new(10));

        let (started1, tx1) = getter(cache.clone(), 100).await;
        started1.await;

        let (started2, tx2) = getter(cache.clone(), 200).await;
        started2.await;

        // both loader()'s are now in progress.  Complete them in LIFO.

        assert_eq!(tx2.send(2).await, 2);
        assert_eq!(tx1.send(1).await, 1);
    }

    #[tokio::test]
    async fn invalidate() {
        let cache = Arc::new(AsyncCache::new(10));
        let key = 123;

        // kick off get, which will complete when we tx.send()
        let (started, tx) = getter(cache.clone(), key).await;
        started.await;

        // invalidate
        cache.invalidate(key);

        // complete get
        assert_eq!(tx.send(1).await, 1);

        // new get does not see cached value because it was invalidated
        let nv = cache.get(key, |_| future::ready(Ok(2))).await.unwrap();
        assert_eq!(nv, 2);
    }

    #[tokio::test]
    async fn invalidate_double_get() {
        let cache = Arc::new(AsyncCache::new(10));
        let key = 123;

        // kick off first get, which will be invalidated
        let (started1, tx1) = getter(cache.clone(), key).await;
        started1.await;

        // invalidate
        cache.invalidate(key);

        // kick off second get, it should wait for first get to complete, but not use the cached
        // value
        let (_, tx2) = getter(cache.clone(), key).await;

        // complete first get
        assert_eq!(tx1.send(1).await, 1);

        // complete second get, it should not see a cached value
        assert_eq!(tx2.send(2).await, 2);

        // new get can see second cached value because it was initiated after the invalidate()
        assert_eq!(cache.get(key, |_| future::ready(Ok(3))).await.unwrap(), 2);
    }
}
