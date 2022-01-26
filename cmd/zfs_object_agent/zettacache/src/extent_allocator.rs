use crate::base_types::DiskId;
use crate::base_types::Extent;
use lazy_static::lazy_static;
use log::*;
use more_asserts::*;
use serde::{Deserialize, Serialize};
use std::cmp::max;
use std::cmp::min;
use std::collections::BTreeMap;
use std::mem;
use std::ops::Bound::Included;
use std::ops::Bound::Unbounded;
use util::get_tunable;
use util::iter_wrapping;
use util::RangeTree;

lazy_static! {
    // XXX maybe this is wasteful for the smaller logs?
    pub static ref DEFAULT_EXTENT_SIZE: u64 = get_tunable("default_extent_size", 128 * 1024 * 1024);
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtentAllocatorPhys {
    pub capacity: Vec<Extent>,
}

impl ExtentAllocatorPhys {
    pub fn new(capacity: Vec<Extent>) -> Self {
        Self { capacity }
    }

    pub fn extend<T>(&mut self, capacity: T)
    where
        T: IntoIterator<Item = Extent>,
    {
        self.capacity.extend(capacity);
    }
}

pub struct ExtentAllocator {
    inner: std::sync::Mutex<Inner>,
}

struct Inner {
    sections: BTreeMap<Extent, Section>,
    next: Extent,
}

struct Section {
    allocatable: RangeTree,
    freeing: RangeTree, // not yet available for reallocation until this checkpoint completes
}

pub struct ExtentAllocatorBuilder {
    allocatable: BTreeMap<Extent, RangeTree>,
}

impl ExtentAllocatorBuilder {
    pub fn new(phys: &ExtentAllocatorPhys) -> ExtentAllocatorBuilder {
        let mut allocatable = BTreeMap::new();
        for extent in &phys.capacity {
            assert_eq!(extent.location.offset % 512, 0);
            assert_eq!(extent.size % 512, 0);
            let mut rt = RangeTree::new();
            rt.add(extent.location.offset, extent.size);
            let existing = allocatable.insert(*extent, rt);
            // extents should not overlap
            assert!(existing.is_none());
        }
        ExtentAllocatorBuilder { allocatable }
    }

    pub fn claim(&mut self, extent: &Extent) {
        get_containing_extent(&mut self.allocatable, extent)
            .remove(extent.location.offset, extent.size);
    }

    pub fn allocatable_bytes(&self) -> u64 {
        self.allocatable.values().map(|rt| rt.space()).sum()
    }
}

impl ExtentAllocator {
    /// Since the on-disk representation doesn't indicate which extents are
    /// allocated, they must all be .claim()ed first, via the
    /// ExtentAllocatorBuilder.
    pub fn open(builder: ExtentAllocatorBuilder) -> ExtentAllocator {
        let sections = builder
            .allocatable
            .into_iter()
            .map(|(capacity, allocatable)| {
                (
                    capacity,
                    Section {
                        allocatable,
                        freeing: Default::default(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        ExtentAllocator {
            inner: std::sync::Mutex::new(Inner {
                next: sections.keys().next().copied().unwrap(),
                sections,
            }),
        }
    }

    pub fn get_phys(&self) -> ExtentAllocatorPhys {
        ExtentAllocatorPhys {
            capacity: self
                .inner
                .lock()
                .unwrap()
                .sections
                .keys()
                .copied()
                .collect(),
        }
    }

    pub fn allocatable_bytes(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .sections
            .values()
            .map(|section| section.allocatable.space())
            .sum()
    }

    pub fn checkpoint_done(&self) {
        let mut inner = self.inner.lock().unwrap();

        for section in inner.sections.values_mut() {
            // Space freed during this checkpoint is now available for reallocation.
            for (&start, &size) in mem::take(&mut section.freeing).iter() {
                assert_eq!(start % 512, 0);
                assert_eq!(size % 512, 0);
                section.allocatable.add(start, size);
            }
        }
    }

    pub fn allocate(&self, min_size: u64) -> Extent {
        let mut inner = self.inner.lock().unwrap();

        // find first segment where this fits, or largest free segment.
        // XXX keep size-sorted tree as well?
        let mut best_extent: Option<Extent> = None;
        for (extent, section) in iter_wrapping(&inner.sections, inner.next) {
            for (&offset, &size) in section.allocatable.iter() {
                if size > min_size && size > best_extent.map_or(0, |extent| extent.size) {
                    // We use the default extent size (128MB), but not more than
                    // 1/128th of the free extent, and at least the requested
                    // minimum size.  Note that the size needs to be
                    // sector-aligned. The min_size, extent.size, and
                    // DEFAULT_EXTENT_SIZE are assumed to be multiples of the
                    // sector size.  However, when dividing by 128, the result
                    // may not be sector-aligned.  Unfortunately this layer
                    // doesn't have access to the actual sector size, so we
                    // align to a multiple of the largest supported sector size,
                    // 4KB.
                    let max_size = max(
                        min_size,
                        min(*DEFAULT_EXTENT_SIZE, (extent.size / 128) & !(4095)),
                    );
                    best_extent = Some(Extent::new(
                        extent.location.disk,
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

        // advance cursor
        inner.next = iter_wrapping(&inner.sections, extent.location)
            .take(2)
            .last()
            .unwrap()
            .0
            .to_owned();

        // remove segment from allocatable
        get_containing_extent(&mut inner.sections, &extent)
            .allocatable
            .remove(extent.location.offset, extent.size);

        debug!("allocated {:?} for min={}", extent, min_size);
        extent
    }

    /// extent can be a subset of what was previously allocated
    pub fn free(&self, extent: &Extent) {
        let mut inner = self.inner.lock().unwrap();
        get_containing_extent(&mut inner.sections, extent)
            .freeing
            .add(extent.location.offset, extent.size);
    }

    // Returns a <disk id> -> <(used bytes, total bytes)> map
    pub fn zcachedb_metadata_per_disk(&self) -> BTreeMap<DiskId, (u64, u64)> {
        let mut map: BTreeMap<DiskId, (u64, u64)> = BTreeMap::new();
        for (extent, section) in &self.inner.lock().unwrap().sections {
            let section_total = extent.size;
            let section_used = section_total - section.allocatable.space();
            let entry = map.entry(extent.location.disk).or_default();
            *entry = (entry.0 + section_used, entry.1 + section_total);
        }
        map
    }
}

fn get_containing_extent<'a, V>(map: &'a mut BTreeMap<Extent, V>, extent: &Extent) -> &'a mut V {
    let (e, v) = map
        .range_mut((Unbounded, Included(extent.location)))
        .next_back()
        .unwrap_or_else(|| panic!("map does not contain {:?}", extent));
    assert!(e.contains(extent), "map does not contain {:?}", extent);
    v
}
