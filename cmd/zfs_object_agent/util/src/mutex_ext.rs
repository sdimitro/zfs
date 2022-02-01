use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;

pub struct NonSendMutexGuard<'a, T> {
    inner: tokio::sync::MutexGuard<'a, T>,
    // force this to not be Send
    _marker: PhantomData<*const ()>,
}

impl<'a, T> Deref for NonSendMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T> DerefMut for NonSendMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// This locks the mutex like Mutex::lock(), but returns a new kind of guard
// which can not be sent between threads.  This is useful if you want to ensure
// that .await is not used while the mutex is locked by some callers, but .await
// can be used from other callers (that use tokio::sync::Mutex::lock()
// directly).
pub async fn lock_non_send<T>(mutex: &tokio::sync::Mutex<T>) -> NonSendMutexGuard<'_, T> {
    // It would be nice to do this via an async_trait, but that requires a memory
    // allocation each time it's called, which can impact performance.
    NonSendMutexGuard {
        inner: mutex.lock().await,
        _marker: PhantomData,
    }
}
