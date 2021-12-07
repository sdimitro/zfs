use crate::base_types::DiskId;
use crate::base_types::Extent;
use log::*;
use more_asserts::*;
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::collections::BTreeMap;
use std::mem;
use util::iter_wrapping;
use util::RangeTree;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtentAllocatorPhys {
    pub capacity: Vec<Extent>,
}

impl ExtentAllocatorPhys {
    pub fn new(capacity: Vec<Extent>) -> Self {
        Self { capacity }
    }
}

pub struct ExtentAllocator {
    inner: std::sync::Mutex<ExtentAllocatorInner>,
}

struct ExtentAllocatorInner {
    disks: BTreeMap<DiskId, ExtentAllocatorDisk>,
    next: DiskId,
}

struct ExtentAllocatorDisk {
    capacity: Extent,
    allocatable: RangeTree,
    freeing: RangeTree, // not yet available for reallocation until this checkpoint completes
}

pub struct ExtentAllocatorBuilder {
    allocatable: BTreeMap<DiskId, (Extent, RangeTree)>,
}

impl ExtentAllocatorBuilder {
    pub fn new(phys: &ExtentAllocatorPhys) -> ExtentAllocatorBuilder {
        let mut allocatable = BTreeMap::new();
        for extent in &phys.capacity {
            assert_eq!(extent.location.offset % 512, 0);
            assert_eq!(extent.size % 512, 0);
            let mut rt = RangeTree::new();
            rt.add(extent.location.offset, extent.size);
            let existing = allocatable.insert(extent.location.disk, (*extent, rt));
            // phys must have at most one extent per disk
            assert!(existing.is_none());
        }
        ExtentAllocatorBuilder { allocatable }
    }

    pub fn claim(&mut self, extent: &Extent) {
        self.allocatable
            .get_mut(&extent.location.disk)
            .unwrap()
            .1
            .remove(extent.location.offset, extent.size);
    }

    pub fn allocatable_bytes(&self) -> u64 {
        self.allocatable.iter().map(|(_, (_, rt))| rt.space()).sum()
    }
}

impl ExtentAllocatorInner {
    fn iter_disks(&self) -> impl Iterator<Item = &ExtentAllocatorDisk> {
        iter_wrapping(&self.disks, self.next)
    }
}

impl ExtentAllocator {
    /// Since the on-disk representation doesn't indicate which extents are
    /// allocated, they must all be .claim()ed first, via the
    /// ExtentAllocatorBuilder.
    pub fn open(builder: ExtentAllocatorBuilder) -> ExtentAllocator {
        let disks: BTreeMap<DiskId, ExtentAllocatorDisk> = builder
            .allocatable
            .into_iter()
            .map(|(disk, (capacity, allocatable))| {
                (
                    disk,
                    ExtentAllocatorDisk {
                        capacity,
                        allocatable,
                        freeing: Default::default(),
                    },
                )
            })
            .collect();
        ExtentAllocator {
            inner: std::sync::Mutex::new(ExtentAllocatorInner {
                next: disks.iter().next().map(|(&disk, _)| disk).unwrap(),
                disks,
            }),
        }
    }

    pub fn get_phys(&self) -> ExtentAllocatorPhys {
        ExtentAllocatorPhys {
            capacity: self
                .inner
                .lock()
                .unwrap()
                .disks
                .iter()
                .map(|(&id, disk)| {
                    assert_eq!(id, disk.capacity.location.disk);
                    disk.capacity
                })
                .collect(),
        }
    }

    pub fn allocatable_bytes(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .disks
            .iter()
            .map(|(_, disk)| disk.allocatable.space())
            .sum()
    }

    pub fn checkpoint_done(&self) {
        let mut inner = self.inner.lock().unwrap();

        for (&id, disk) in inner.disks.iter_mut() {
            assert_eq!(id, disk.capacity.location.disk);
            // Space freed during this checkpoint is now available for reallocation.
            for (&start, &size) in mem::take(&mut disk.freeing).iter() {
                assert_eq!(start % 512, 0);
                assert_eq!(size % 512, 0);
                disk.allocatable.add(start, size);
            }
        }
    }

    pub fn allocate(&self, min_size: u64, max_size: u64) -> Extent {
        let mut inner = self.inner.lock().unwrap();

        // find first segment where this fits, or largest free segment.
        // XXX keep size-sorted tree as well?
        let mut best_extent: Option<Extent> = None;
        for disk in inner.iter_disks() {
            for (&offset, &size) in disk.allocatable.iter() {
                if size > min_size && size > best_extent.map_or(0, |extent| extent.size) {
                    best_extent = Some(Extent::new(
                        disk.capacity.location.disk,
                        offset,
                        min(size, max_size),
                    ));
                    if size >= max_size {
                        break;
                    }
                }
            }
        }
        let extent = best_extent
            .unwrap_or_else(|| panic!("no free metadata chunk of at least {}KB", min_size / 1024));
        assert_ge!(extent.size, min_size);
        assert_le!(extent.size, max_size);

        // advance cursor
        inner.next = iter_wrapping(&inner.disks, extent.location.disk)
            .take(2)
            .last()
            .unwrap()
            .capacity
            .location
            .disk;

        // remove segment from allocatable
        inner
            .disks
            .get_mut(&extent.location.disk)
            .unwrap()
            .allocatable
            .remove(extent.location.offset, extent.size);

        debug!(
            "allocated {:?} for min={} max={}",
            extent, min_size, max_size
        );
        extent
    }

    /// extent can be a subset of what was previously allocated
    pub fn free(&self, extent: &Extent) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .disks
            .get_mut(&extent.location.disk)
            .unwrap()
            .freeing
            .add(extent.location.offset, extent.size);
    }
}
