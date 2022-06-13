use std::cmp::max;
use std::sync::Arc;

use tokio::sync::RwLock;
use util::with_alloctag;
use util::zettacache_stats::DiskIoType;
use util::AlignedVec;

use crate::base_types::DiskLocation;
use crate::block_access::BlockAccess;

#[derive(Debug)]
#[must_use]
pub struct AggregatingWriter {
    block_access: Arc<BlockAccess>,
    capacity: usize,
    pending: Option<PendingWrite>,
    flushes: Arc<RwLock<()>>,
}

#[derive(Debug)]
struct PendingWrite {
    location: DiskLocation,
    vec: AlignedVec,
}

impl AggregatingWriter {
    pub fn new(block_access: Arc<BlockAccess>, capacity: usize) -> Self {
        Self {
            block_access,
            capacity,
            pending: None,
            flushes: Default::default(),
        }
    }

    pub fn write(&mut self, location: DiskLocation, data: &[u8]) {
        loop {
            let pending = match &mut self.pending {
                Some(pending) => pending,
                None => {
                    let mut vec = with_alloctag("AggregatingWriter PendingWrite", || {
                        AlignedVec::with_capacity(
                            max(self.capacity, data.len()),
                            self.block_access.round_up_to_sector(1),
                        )
                    });
                    vec.extend_from_slice(data);
                    self.pending = Some(PendingWrite { location, vec });
                    return;
                }
            };
            if pending.location + pending.vec.len() == location
                && pending.vec.unused_capacity() >= data.len()
            {
                pending.vec.extend_from_slice(data);
                return;
            } else {
                self.initiate_flush();
            }
        }
    }

    fn initiate_flush(&mut self) {
        if let Some(pending) = self.pending.take() {
            assert!(!pending.vec.is_empty());
            let block_access = self.block_access.clone();
            // Take the reader lock so that flush() can wait for the spawned task to complete.
            // The unwrap is safe because the writer lock is only taken by flush(), which has
            // exclusive access to the AggregatingWriter, so we can't be concurrently calling
            // initiate_flush().
            let guard = self.flushes.clone().try_read_owned().unwrap();
            tokio::spawn(async move {
                block_access
                    .write_raw(
                        pending.location,
                        pending.vec.into(),
                        DiskIoType::MaintenanceWrite,
                    )
                    .await;
                drop(guard);
            });
        }
    }

    pub async fn flush(mut self) {
        self.initiate_flush();
        // Obtain the flushes write lock to wait for all read lock holders (in progress flush
        // tasks) to complete and drop the read locks.
        self.flushes.write().await;
    }
}

impl Drop for AggregatingWriter {
    fn drop(&mut self) {
        if self.pending.is_some() || self.flushes.try_write().is_err() {
            panic!("AggregatingWriter dropped without flushing");
        }
    }
}
