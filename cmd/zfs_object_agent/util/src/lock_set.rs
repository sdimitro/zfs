use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use log::*;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use tokio::sync::watch;

use crate::super_trace;

#[derive(Default, Debug, Clone)]
pub struct LockSet<V: Hash + Eq + Copy + Debug> {
    locks: Arc<DashMap<V, watch::Receiver<()>>>,
}

pub struct LockedItem<V: Hash + Eq + Copy + Debug> {
    value: V,
    tx: watch::Sender<()>,
    set: LockSet<V>,
}

impl<V: Hash + Eq + Copy + Debug> Drop for LockedItem<V> {
    fn drop(&mut self) {
        super_trace!("{:?}: removing lock", self.value);
        let rx = self.set.locks.remove(&self.value);
        assert!(rx.is_some());
        // This unwrap can't fail because there is still a receiver, `rx`.
        self.tx.send(()).unwrap();
    }
}

impl<V: Hash + Eq + Copy + Debug> LockedItem<V> {
    pub fn value(&self) -> &V {
        &self.value
    }
}

impl<V: Hash + Eq + Copy + Debug> LockSet<V> {
    pub fn new() -> Self {
        Self {
            locks: Default::default(),
        }
    }

    pub async fn lock(&self, value: V) -> LockedItem<V> {
        let tx = loop {
            let mut rx = {
                match self.locks.entry(value) {
                    Entry::Occupied(oe) => oe.get().clone(),
                    Entry::Vacant(ve) => {
                        let (tx, rx) = watch::channel(());
                        ve.insert(rx);
                        break tx;
                    }
                }
            };
            super_trace!("{:?}: waiting for existing lock", value);
            // Note: since we don't hold the locks mutex now, the corresponding
            // LockedItem may have been dropped, in which case the sender was
            // dropped.  In this case, the changed() Result will be an Err,
            // which we ignore with ok().
            rx.changed().await.ok();
        };
        super_trace!("{:?}: inserted new lock", value);

        LockedItem {
            value,
            tx,
            set: self.clone(),
        }
    }
}
