use crate::base_types::*;
use crate::block_access::*;
use crate::extent_allocator::ExtentAllocator;
use crate::range_tree::RangeTree;
use crate::space_map::SpaceMap;
use crate::space_map::SpaceMapPhys;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub struct BlockAllocator {
    space_map: SpaceMap,
    allocatable: RangeTree,
    allocating: RangeTree,
    freeing: RangeTree,
    last_location: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockAllocatorPhys {
    allocatable: SpaceMapPhys,
}
impl OnDisk for BlockAllocatorPhys {}

impl BlockAllocatorPhys {
    // XXX eventually change this to indicate the size of each of the disks that we're managing
    pub fn new(offset: u64, size: u64) -> BlockAllocatorPhys {
        BlockAllocatorPhys {
            allocatable: SpaceMapPhys::new(offset, size),
        }
    }
}

impl BlockAllocator {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: BlockAllocatorPhys,
    ) -> BlockAllocator {
        let space_map = SpaceMap::open(
            block_access.clone(),
            extent_allocator.clone(),
            phys.allocatable,
        );
        BlockAllocator {
            allocatable: space_map.load().await,
            space_map,
            allocating: Default::default(),
            freeing: Default::default(),
            last_location: 0,
        }
    }

    pub async fn flush(&mut self) -> BlockAllocatorPhys {
        // Space freed during this checkpoint is now available for reallocation.
        for (&start, &size) in self.freeing.iter() {
            self.allocating.verify_absent(start, size);
            self.space_map.free(start, size);
            self.allocatable.add(start, size);
        }
        self.freeing.clear();
        for (&start, &size) in self.allocating.iter() {
            self.allocatable.verify_absent(start, size);
            self.space_map.alloc(start, size);
        }
        self.allocating.clear();

        BlockAllocatorPhys {
            allocatable: self.space_map.flush().await,
        }
    }

    fn allocate_impl(&mut self, size: u64, min_location: u64, max_location: u64) -> Option<Extent> {
        for (&allocatable_offset, &allocatable_size) in
            self.allocatable.range(min_location..max_location)
        {
            if allocatable_size >= size {
                self.freeing.verify_absent(allocatable_offset, size);
                self.allocatable.remove(allocatable_offset, size);
                self.allocating.add(allocatable_offset, size);
                self.last_location = allocatable_offset + size;
                return Some(Extent {
                    location: DiskLocation {
                        offset: allocatable_offset,
                    },
                    size: size as usize,
                });
            }
        }
        None
    }

    pub fn allocate(&mut self, size: u64) -> Option<Extent> {
        // find next segment where this fits, or largest free segment.
        // XXX keep size-sorted tree as well?
        match self.allocate_impl(size, self.last_location, u64::MAX) {
            Some(e) => Some(e),
            None => self.allocate_impl(size, 0, self.last_location),
        }
    }

    pub fn free(&mut self, extent: &Extent) {
        self.allocatable
            .verify_absent(extent.location.offset, extent.size as u64);
        self.allocating
            .verify_absent(extent.location.offset, extent.size as u64);
        self.freeing.add(extent.location.offset, extent.size as u64);
    }

    // XXX this is O(N); should make the RangeTree keep the sum
    pub fn get_available(&self) -> u64 {
        self.allocatable.iter().map(|(_offset, size)| size).sum()
    }

    // XXX this is O(N); should make the RangeTree keep the sum
    pub fn get_freeing(&self) -> u64 {
        self.freeing.iter().map(|(_offset, size)| size).sum()
    }
}
