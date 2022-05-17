use std::collections::BTreeSet;
use std::ops::Bound::*;
use std::sync::RwLock;
use std::time::Instant;

use futures::future;
use futures::StreamExt;
use log::*;
use more_asserts::*;
use serde::Deserialize;
use serde::Serialize;
use util::with_alloctag;
use zettacache::base_types::*;

use crate::base_types::*;
use crate::object_based_log::ObjectBasedLog;
use crate::object_based_log::ObjectBasedLogEntry;

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
// XXX make this private and make methods for everything that uses it
pub enum StorageObjectLogEntry {
    Alloc { object: ObjectId },
    Free { object: ObjectId },
}
impl ObjectBasedLogEntry for StorageObjectLogEntry {}

#[derive(Debug)]
pub struct ObjectBlockMap {
    state: RwLock<ObjectBlockMapState>,
}

#[derive(Debug)]
struct ObjectBlockMapState {
    map: BTreeSet<ObjectId>,
    next_block: BlockId,
}

impl ObjectBlockMap {
    const MAP_TAG: &'static str = "ObjectBlockMap.state.map";

    pub async fn load(
        storage_object_log: &ObjectBasedLog<StorageObjectLogEntry>,
        next_block: BlockId,
    ) -> Self {
        let begin = Instant::now();
        let mut num_alloc_entries: u64 = 0;
        let mut num_free_entries: u64 = 0;
        let mut map: BTreeSet<ObjectId> = BTreeSet::new();
        storage_object_log
            .iterate()
            .for_each(|ent| {
                match ent {
                    StorageObjectLogEntry::Alloc { object } => {
                        let inserted = with_alloctag(Self::MAP_TAG, || map.insert(object));
                        assert!(inserted);
                        num_alloc_entries += 1;
                    }
                    StorageObjectLogEntry::Free { object } => {
                        let removed = map.remove(&object);
                        assert!(removed);
                        num_free_entries += 1;
                    }
                }

                future::ready(())
            })
            .await;
        info!(
            "loaded mapping from {} objects with {} allocs and {} frees in {}ms",
            storage_object_log.num_chunks,
            num_alloc_entries,
            num_free_entries,
            begin.elapsed().as_millis()
        );

        ObjectBlockMap {
            state: RwLock::new(ObjectBlockMapState { map, next_block }),
        }
    }

    pub fn insert(&self, object: ObjectId, next_block: BlockId) {
        // verify that this object and block are after the last
        let mut state = self.state.write().unwrap();
        assert_lt!(object.as_min_block(), next_block);
        assert_eq!(object.as_min_block(), state.next_block);
        if let Some(&last_object) = state.map.iter().next_back() {
            assert_gt!(object, last_object);
        }

        let inserted = with_alloctag(Self::MAP_TAG, || state.map.insert(object));
        assert!(inserted, "{:?} is already in the ObjectBlockMap", object);
        state.next_block = next_block;
    }

    pub fn remove(&self, object: ObjectId) {
        let mut state = self.state.write().unwrap();
        let removed = state.map.remove(&object);
        assert!(removed);
    }

    pub fn block_to_object(&self, block: BlockId) -> ObjectId {
        let state = self.state.read().unwrap();
        assert_lt!(block, state.next_block);
        *state
            .map
            .range((Unbounded, Included(ObjectId::new(block))))
            .next_back()
            .unwrap()
    }

    pub fn object_to_next_block(&self, object: ObjectId) -> BlockId {
        let state = self.state.read().unwrap();

        // The "next block" (i.e. the first BlockID that's not valid in this
        // object) is the next object's first block.  Or if this is the last
        // object, it's the next block of the entire pool (state.next_block).
        match state.map.range((Excluded(object), Unbounded)).next() {
            Some(next_object) => next_object.as_min_block(),
            None => state.next_block,
        }
    }

    pub fn last_object(&self) -> ObjectId {
        let state = self.state.read().unwrap();
        *state
            .map
            .iter()
            .next_back()
            .unwrap_or(&ObjectId::new(BlockId(0)))
    }

    pub fn next_object(&self) -> ObjectId {
        ObjectId::new(self.state.read().unwrap().next_block)
    }

    pub fn len(&self) -> usize {
        let state = self.state.read().unwrap();
        state.map.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        let state = self.state.read().unwrap();
        state.map.is_empty()
    }

    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(ObjectId),
    {
        let state = self.state.read().unwrap();
        for &object in state.map.iter() {
            f(object);
        }
    }
}
