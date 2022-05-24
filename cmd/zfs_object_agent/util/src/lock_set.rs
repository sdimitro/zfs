use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use derivative::Derivative;

use crate::super_trace;
use crate::watch_once;

#[derive(Derivative, Clone)]
#[derivative(Default(bound = ""))]
pub struct LockSet<V: Hash + Eq + Copy + Debug> {
    locks: Arc<DashMap<V, watch_once::Receiver<()>>>,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct LockedItem<V: Hash + Eq + Copy + Debug> {
    value: V,
    #[derivative(Debug = "ignore")]
    _tx: watch_once::Sender<()>,
    #[derivative(Debug = "ignore")]
    set: LockSet<V>,
}

impl<V: Hash + Eq + Copy + Debug> Drop for LockedItem<V> {
    fn drop(&mut self) {
        super_trace!("{:?}: removing lock", self.value);
        self.set.locks.remove(&self.value).unwrap();
        // self._tx is dropped here, waking any waiting receivers.
    }
}

impl<V: Hash + Eq + Copy + Debug> LockedItem<V> {
    pub fn value(&self) -> &V {
        &self.value
    }
}

impl<V: Hash + Eq + Copy + Debug> LockSet<V> {
    pub async fn lock(&self, value: V) -> LockedItem<V> {
        let tx = loop {
            let rx = {
                match self.locks.entry(value) {
                    Entry::Occupied(oe) => oe.get().clone(),
                    Entry::Vacant(ve) => {
                        let (tx, rx) = watch_once::channel();
                        ve.insert(rx);
                        break tx;
                    }
                }
            };
            super_trace!("{:?}: waiting for existing lock", value);
            // Note: the sender always drops, resulting in an Err, which we ignore with ok().
            rx.recv().await.ok();
        };
        super_trace!("{:?}: inserted new lock", value);

        LockedItem {
            value,
            _tx: tx,
            set: self.clone(),
        }
    }
}
