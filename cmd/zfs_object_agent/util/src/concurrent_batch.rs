use std::future::Future;
use std::mem;
use std::sync::Arc;

use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::RwLock;

#[derive(Debug, Default)]
pub struct ConcurrentBatch {
    inner: Arc<RwLock<()>>,
}

pub struct ConcurrentBatchGuard(OwnedRwLockReadGuard<()>);

/// This synchronization primitive allows for waiting for a batch of operations to complete,
/// while concurrently allowing more operations to begin.
impl ConcurrentBatch {
    pub fn new() -> Self {
        Default::default()
    }

    /// Start an operation, returning a RAII guard which will mark the operation as completed
    /// when it is dropped.
    pub fn acquire(&self) -> ConcurrentBatchGuard {
        // There is no writer holding or waiting for the lock, because the writer can only be
        // acquired after changing self.inner to a new instance, which requires exclusive access
        // to the ConcurrentBatch.
        ConcurrentBatchGuard(self.inner.clone().try_read_owned().unwrap())
    }

    /// Start a new batch of operations.  Returns a future which will wait for the previous batch
    /// to complete.  Typical use would be to await the future after dropping the lock that
    /// protects the ConcurrentBatch.
    /// ```
    /// use util::concurrent_batch::ConcurrentBatch;
    /// use std::sync::Mutex;
    /// let container: Mutex<ConcurrentBatch> = Default::default();
    /// let guard = container.lock().unwrap().acquire();
    /// let old_batch = container.lock().unwrap().rotate();
    /// //old_batch.await; // will complete when guard is dropped
    /// ```
    pub fn rotate(&mut self) -> impl Future {
        let old_batch = mem::take(&mut self.inner);
        async move {
            old_batch.write().await;
            // There isn't any way for more readers to acquire Arc's of this instance.  However,
            // OwnedRwLockReadGuard::drop() first manipulates its semaphore to drop the lock
            // (allowing the write() above to complete) and then drops its Arc.  Since we may run
            // before the Arc is dropped, we can't assert that the Arc has no other references.
        }
    }
}
