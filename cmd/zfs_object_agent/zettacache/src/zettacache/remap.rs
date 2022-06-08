use std::collections::BTreeMap;
use std::fmt::Debug;
use std::ops::Bound::Included;
use std::ops::Bound::Unbounded;
use std::sync::Arc;

use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;

use crate::base_types::DiskLocation;
use crate::base_types::Extent;
use crate::block_access::BlockAccess;
use crate::block_based_log::BlockBasedLog;
use crate::block_based_log::BlockBasedLogEntry;
use crate::block_based_log::BlockBasedLogPhys;
use crate::slab_allocator::SlabAccess;
use crate::slab_allocator::SlabAllocator;

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub(super) struct RemapLogEntry {
    old: Extent,
    new: Option<DiskLocation>,
}
impl BlockBasedLogEntry for RemapLogEntry {}

#[derive(Debug)]
pub(super) struct RemapState {
    pub(super) map: BTreeMap<Extent, Option<DiskLocation>>,
    pub(super) log_phys: BlockBasedLogPhys<RemapLogEntry>,
}

impl RemapState {
    pub(super) async fn open(
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
        log_phys: BlockBasedLogPhys<RemapLogEntry>,
    ) -> Self {
        Self {
            map: log_phys
                .iter(block_access, slab_access)
                .map(|entry| (entry.old, entry.new))
                .collect()
                .await,
            log_phys,
        }
    }

    pub(super) async fn create(
        block_access: Arc<BlockAccess>,
        slab_allocator: Arc<SlabAllocator>,
        map: BTreeMap<Extent, Option<DiskLocation>>,
    ) -> Self {
        let mut log =
            BlockBasedLog::<RemapLogEntry>::open(block_access, slab_allocator, Default::default());
        for (&old, &new) in map.iter() {
            log.push(RemapLogEntry { old, new });
        }
        let log_phys = log.flush().await;
        Self { map, log_phys }
    }

    pub(super) fn remap(&self, extent: Extent) -> Option<DiskLocation> {
        if let Some((old, new)) = self
            .map
            .range((Unbounded, Included(extent.location)))
            .next_back()
        {
            if old.contains(&extent) {
                match new {
                    Some(new_location) => {
                        // This represents the offset of the passed in extent, into the extent
                        // that was moved as part of the rebalance operation. For example,
                        // multiple contiguously allocated blocks maybe have been moved via a
                        // single extent. Thus, to remap one of those blocks' to it's new
                        // location on disk, we need this offset (this offset is maintained when
                        // the blocks are copied).
                        let offset = extent.location - old.location;

                        return Some(DiskLocation::new(
                            new_location.disk(),
                            new_location.offset() + offset,
                        ));
                    }
                    None => {
                        // This means the extent was part of a rebalance operation, but when
                        // attempting to remap the old location to a new location, the allocation
                        // failed. Thus, the old extent does not have new location, and it will
                        // be invalid after the rebalance completes.
                        return None;
                    }
                }
            }
        }

        // If we reach this point, we didn't find an extent in the mapping that contains the passed
        // in extent, which means the passed in extent was not remapped; thus, we simply
        // return the old extent's location.
        Some(extent.location)
    }
}
