use crate::base_types::DiskLocation;
use crate::base_types::Extent;
use crate::block_access::*;
use crate::block_allocator::SlabId;
use crate::block_based_log::*;
use crate::extent_allocator::ExtentAllocator;
use crate::{
    base_types::OnDisk,
    block_based_log::{BlockBasedLog, BlockBasedLogEntry},
};
use futures::future;
use futures::stream::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub struct SpaceMapExtent {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub struct MarkGenerationEntry {
    pub slab_id: SlabId,
    pub generation: u64,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub enum SpaceMapEntry {
    Alloc(SpaceMapExtent),
    Free(SpaceMapExtent),
    MarkGeneration(MarkGenerationEntry),
}
impl OnDisk for SpaceMapEntry {}
impl BlockBasedLogEntry for SpaceMapEntry {}

pub struct SpaceMap {
    log: BlockBasedLog<SpaceMapEntry>,
    coverage: SpaceMapExtent,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpaceMapPhys {
    log: BlockBasedLogPhys<SpaceMapEntry>,
    coverage: SpaceMapExtent,
}
impl OnDisk for SpaceMapPhys {}

impl SpaceMapPhys {
    pub fn new(offset: u64, size: u64) -> SpaceMapPhys {
        SpaceMapPhys {
            log: Default::default(),
            coverage: SpaceMapExtent { offset, size },
        }
    }
}

impl SpaceMap {
    pub fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: SpaceMapPhys,
    ) -> SpaceMap {
        SpaceMap {
            log: BlockBasedLog::open(block_access, extent_allocator, phys.log),
            coverage: phys.coverage,
        }
    }

    pub async fn load<F>(&self, mut import_cb: F)
    where
        F: FnMut(SpaceMapEntry),
    {
        self.log
            .iter()
            .for_each(|entry| {
                import_cb(entry);
                future::ready(())
            })
            .await;
    }

    pub fn alloc(&mut self, offset: u64, size: u64) {
        self.log
            .append(SpaceMapEntry::Alloc(SpaceMapExtent { offset, size }));
    }

    pub fn free(&mut self, offset: u64, size: u64) {
        self.log
            .append(SpaceMapEntry::Free(SpaceMapExtent { offset, size }));
    }

    pub fn mark_generation(&mut self, slab_id: SlabId, generation: u64) {
        self.log
            .append(SpaceMapEntry::MarkGeneration(MarkGenerationEntry {
                slab_id,
                generation,
            }));
    }

    pub async fn flush(&mut self) -> SpaceMapPhys {
        SpaceMapPhys {
            log: self.log.flush().await,
            coverage: self.coverage,
        }
    }

    pub fn get_coverage(&self) -> Extent {
        Extent {
            location: DiskLocation {
                offset: self.coverage.offset,
            },
            size: self.coverage.size,
        }
    }
}
