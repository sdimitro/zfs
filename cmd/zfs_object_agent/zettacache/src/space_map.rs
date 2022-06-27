use std::sync::Arc;

use futures::future;
use futures::stream::StreamExt;
use serde::Deserialize;
use serde::Serialize;

use crate::base_types::Extent;
use crate::block_access::BlockAccess;
use crate::block_allocator::SlabPhysType;
use crate::block_based_log::BlockBasedLog;
use crate::block_based_log::BlockBasedLogEntry;
use crate::block_based_log::BlockBasedLogPhys;
use crate::slab_allocator::SlabAccess;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::slab_allocator::SlabId;

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub enum SpaceMapEntry {
    Alloc(Extent),
    Free(Extent),
    SlabInfo(SlabId, SlabPhysType),
}
impl BlockBasedLogEntry for SpaceMapEntry {}

pub struct SpaceMap {
    log: BlockBasedLog<SpaceMapEntry>,
    // This is only used currently for printing out the ideal size that the
    // spacemap would have if it was condensed to our logs.
    alloc_entries: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpaceMapPhys {
    log: BlockBasedLogPhys<SpaceMapEntry>,
    alloc_entries: u64,
}

impl SpaceMapPhys {
    pub fn new() -> SpaceMapPhys {
        SpaceMapPhys {
            log: Default::default(),
            alloc_entries: 0,
        }
    }

    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.log.claim(builder);
    }

    pub fn bytes(&self) -> u64 {
        self.log.bytes()
    }

    pub fn total_entries(&self) -> u64 {
        self.log.len()
    }

    pub async fn load<F>(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
        mut import_cb: F,
    ) where
        F: FnMut(SpaceMapEntry),
    {
        self.log
            .iter(block_access, slab_access)
            .for_each(|entry| {
                import_cb(entry);
                future::ready(())
            })
            .await;
    }
}

impl SpaceMap {
    pub fn open(
        block_access: Arc<BlockAccess>,
        slab_allocator: Arc<SlabAllocator>,
        phys: SpaceMapPhys,
    ) -> SpaceMap {
        SpaceMap {
            log: BlockBasedLog::open(block_access, slab_allocator, phys.log),
            alloc_entries: phys.alloc_entries,
        }
    }

    pub fn alloc(&mut self, extent: Extent) {
        if extent.size != 0 {
            self.log.push(SpaceMapEntry::Alloc(extent));
            self.alloc_entries += 1;
        }
    }

    pub fn free(&mut self, extent: Extent) {
        if extent.size != 0 {
            self.log.push(SpaceMapEntry::Free(extent));
        }
    }

    pub fn mark_slab_info(&mut self, slab_id: SlabId, slab_type: SlabPhysType) {
        self.log.push(SpaceMapEntry::SlabInfo(slab_id, slab_type));
    }

    pub async fn flush(&mut self) -> SpaceMapPhys {
        SpaceMapPhys {
            log: self.log.flush().await,
            alloc_entries: self.alloc_entries,
        }
    }

    pub fn total_entries(&self) -> u64 {
        self.log.len()
    }

    pub fn alloc_entries(&self) -> u64 {
        self.alloc_entries
    }

    pub fn pending_len(&self) -> u64 {
        self.log.pending_len()
    }

    pub fn bytes(&self) -> u64 {
        self.log.num_bytes()
    }

    pub fn clear(&mut self) {
        self.log.clear();
        self.alloc_entries = 0;
    }
}
