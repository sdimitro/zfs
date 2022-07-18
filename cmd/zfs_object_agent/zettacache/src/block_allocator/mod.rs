mod slabs;
pub mod zcdb;

use std::cmp::max;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::mem;
use std::ops::Bound::*;
use std::sync::Arc;
use std::time::Instant;

use bytesize::ByteSize;
use derivative::Derivative;
use either::Either;
use log::*;
use more_asserts::*;
use num_traits::cast::ToPrimitive;
use serde::Deserialize;
use serde::Serialize;
use util::nice_number_count;
use util::nice_p2size;
use util::super_trace;
use util::tunable;
use util::tunable_convert_noop;
use util::with_alloctag;
use util::writeln_stdout;
use util::BitRange;
use util::RangeTree;
use util::VecMap;

use self::slabs::Slabs;
use crate::base_types::*;
use crate::block_access::BlockAccess;
use crate::slab_allocator::SlabAccess;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::slab_allocator::SlabId;
use crate::slab_allocator::DEFAULT_SLAB_SIZE;
use crate::space_map::SpaceMap;
use crate::space_map::SpaceMapPhys;

tunable_convert_noop!(SlabAllocationBucketsPhys);
tunable! {
    static ref DEFAULT_SLAB_BUCKETS: SlabAllocationBucketsPhys =
        SlabAllocationBucketsPhys::default();

    //
    // The rate that we condense our slabs every checkpoint is guided by three factors; we
    // will condense the maximum calculated by each of these:
    //
    // [1] The Incoming Rate Heuristic
    //
    // This is based on the reasoning that the more incoming changes we have the quicker
    // the spacemaps become inefficient from the spacemap entries of these changes. We
    // record the size of the incoming changes for each specific checkpoint (currently
    // measured as the bytes allocated by the block allocator within a checkpoint) and
    // approximate how many slabs these changes can fill up. Finally we multiply the
    // result with a tunable factor and end up with the following formula:
    //
    // slabs_to_condense = SLAB_CONDENSE_RATE_FACTOR * (bytes_written_this_checkpoint / slab_size)
    //
    // There are a few things to consider on the above formula:
    // * It doesn't take into account the bytes freed by the allocator, even though these
    //   add entries to the spacemaps too. Frees can be very bursty depending on how
    //   merging/eviction works. Given that this is an LRU cache though and we want to
    //   model the rate of change, we assume that ingestion (allocs) and evictions (frees)
    //   are coupled in the long term.
    // * An alternative and potentially more accurate measurement for rate of change in our
    //   spacemaps could be the number of spacemap entries to be appended every checkpoint.
    //   The problem with this measurement though is that it's trickier to tie back to the
    //   question of how many slabs we should condense this checkpoint (especially given our
    //   two spacemap scheme).
    //
    // [2] The Minimum Tunable (SLAB_CONDENSE_MIN_PER_CHECKPOINT)
    //
    // This tunable exists to make sure that we're still doing some condensing work even
    // when the incoming rate of changes is low. It currently represents the minimum number
    // of slabs that we want to condense every checkpoint. The reasoning behind making this
    // tunable an absolute number, and not say a percentage of the total number of slabs,
    // is that we can at least keep the CPU runtime of condensing constant regardless of
    // the cache's size. This means that bigger pools may need more time to condense their
    // spacemaps but at least the CPU overhead will stay the same (which currently seems the
    // right trade-off as storage space is easier to increase compared to CPU speeds). Besides
    // that, our experience with spacemaps so far is that they are still relatively small
    // compared to other structures in the metadata space.
    //
    // [3] The Maximum Badness Ratio
    //
    // This is based on the reasoning that we don't want the spacemaps to be a lot worse than
    // what's "optimal".  The optimal size is set by the number of disjoint free segments at
    // the time the last merge completed.  Between merges, allocations tend to fill in holes
    // and decrease the number of segments, but we aren't really interested in capturing this
    // potential improvement.  If the size of the old spacemap ever reaches >20x optimal, we
    // will condense all slabs in one checkpoint.  Otherwise we condense a fraction of the
    // slabs that's based on (badness / MAX_BADNESS).
    //
    // In practice, on demanding workloads, the Max Badness metric will dominate (i.e. tell us
    // to condense more than the other metrics).
    static ref SLAB_CONDENSE_RATE_FACTOR: f64 = 2.0;
    static ref SLAB_CONDENSE_MIN_PER_CHECKPOINT: usize = 1000;
    static ref SLAB_CONDENSE_MAX_BADNESS_RATIO: f64 = 20.0;
    static ref SLAB_CONDENSE_MIN_BADNESS_ENTRIES: u64 = 1_000_000;
}

#[derive(Clone, Copy, Debug, Hash, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct SlabBucketSize(u32);

trait SlabTrait {
    fn import_alloc(&mut self, extent: Extent);
    fn import_free(&mut self, extent: Extent);
    fn allocate(&mut self, size: u32) -> Option<Extent>;
    fn free(&mut self, extent: Extent);
    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) -> (u64, u64);
    fn condense_to_spacemap(&self, spacemap: &mut SpaceMap);
    fn mark_slab_info(&self, id: SlabId, spacemap: &mut SpaceMap);
    fn max_size(&self) -> u32;
    fn capacity_bytes(&self) -> u64;
    fn free_space(&self) -> u64;
    fn freeing_space(&self) -> u64;
    fn allocated_space(&self) -> u64;
    fn num_segments(&self) -> u64;
    fn allocated_extents(&self) -> Vec<Extent>;
    fn dump_info(&self);
    fn location(&self) -> DiskLocation;
}

struct BitmapSlab {
    allocatable: BitRange,
    allocating: BitRange,
    freeing: BitRange,

    total_slots: u16,
    slot_size: u32,
    location: DiskLocation,
}

impl BitmapSlab {
    const ALLOCATABLE_TAG: &'static str = "BitmapSlab.allocatable";

    fn new_slab(id: SlabId, extent: Extent, block_size: u32) -> Slab {
        let slab_size = u32::try_from(extent.size).unwrap();
        let num_slots = slab_size / block_size;
        let free_slots = if num_slots > u16::MAX.into() {
            // Since the default slab_size is 32MB, num_slots can overflow a
            // u16 for 512-byte slots.
            assert_ge!(u64::from(slab_size), ByteSize::mib(32).as_u64());
            u16::MAX
        } else {
            u16::try_from(num_slots).unwrap()
        };
        let mut allocatable = BitRange::new();

        allocatable.insert_range(0..free_slots);
        assert_eq!(allocatable.len(), free_slots);

        Slab::new(
            id,
            SlabEnum::BitmapBased(BitmapSlab {
                allocatable,
                allocating: Default::default(),
                freeing: Default::default(),
                total_slots: free_slots,
                slot_size: block_size,
                location: extent.location,
            }),
        )
    }

    fn slot_to_location(&self, slot: u16) -> DiskLocation {
        self.location + u64::from(slot) * u64::from(self.slot_size)
    }

    fn slab_end(&self) -> DiskLocation {
        self.slot_to_location(self.total_slots)
    }

    fn verify_contains(&self, extent: Extent) {
        assert_eq!(extent.location.disk(), self.location.disk());
        assert_eq!(extent.size % u64::from(self.max_size()), 0);
        assert_ge!(extent.location, self.location);
        assert_le!(extent.location + extent.size, self.slab_end());
    }

    fn import_extent_impl(&mut self, extent: Extent, is_alloc: bool) {
        self.verify_contains(extent);

        let internal_offset = u32::try_from(extent.location - self.location).unwrap();
        assert_eq!(internal_offset % self.slot_size, 0);
        let num_slots = u16::try_from(extent.size / u64::from(self.slot_size)).unwrap();
        assert_ge!(num_slots, 1);

        let first_slot = u16::try_from(internal_offset / self.slot_size).unwrap();
        assert_le!(
            first_slot + num_slots,
            self.total_slots,
            "import range crosses the slab's end boundary"
        );
        let slot_range = first_slot..(first_slot + num_slots);
        if is_alloc {
            with_alloctag(Self::ALLOCATABLE_TAG, || {
                self.allocatable.remove_range(slot_range)
            });
        } else {
            with_alloctag(Self::ALLOCATABLE_TAG, || {
                self.allocatable.insert_range(slot_range)
            });
        }
    }
}

impl SlabTrait for BitmapSlab {
    fn import_alloc(&mut self, extent: Extent) {
        self.import_extent_impl(extent, true);
    }

    fn import_free(&mut self, extent: Extent) {
        self.import_extent_impl(extent, false);
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        assert_ge!(self.slot_size, size);
        if self.allocatable.is_empty() {
            return None;
        }

        let slot = self.allocatable.min().unwrap();
        self.allocating.insert(slot);
        with_alloctag(Self::ALLOCATABLE_TAG, || self.allocatable.remove(slot));

        // Cannot be allocating a block that's currently in the middle of being freed.
        assert!(!self.freeing.contains(slot));
        Some(Extent {
            location: self.slot_to_location(slot),
            size: self.slot_size.into(),
        })
    }

    fn free(&mut self, extent: Extent) {
        self.verify_contains(extent);

        let internal_offset = u32::try_from(extent.location - self.location).unwrap();
        assert_eq!(internal_offset % self.slot_size, 0);

        let slot = u16::try_from(internal_offset / self.slot_size).unwrap();
        assert!(
            !self.allocatable.contains(slot),
            "double free at slot {:?}",
            slot
        );
        self.freeing.insert(slot);
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) -> (u64, u64) {
        // It could happen that a segment was allocated and then freed within the same checkpoint
        // period at which point it would be part of both `allocating` and `freeing` sets. For
        // this reason we always record `allocating` first, before `freeing`, on our spacemaps.
        // Note that segments cannot be freed and then allocated within the same checkpoint
        // period.

        let allocated_bytes = u64::from(self.allocatable.len()) * u64::from(self.slot_size);
        for (slot, run) in self.allocating.iter_ranges() {
            spacemap.alloc(Extent {
                location: self.slot_to_location(slot),
                size: u64::from(run) * u64::from(self.slot_size),
            });
        }
        self.allocating.clear();

        // Space freed during this checkpoint is now available for reallocation.
        let freed_bytes = u64::from(self.freeing.len()) * u64::from(self.slot_size);
        for (slot, run) in self.freeing.iter_ranges() {
            spacemap.free(Extent {
                location: self.slot_to_location(slot),
                size: u64::from(run) * u64::from(self.slot_size),
            });
            with_alloctag(Self::ALLOCATABLE_TAG, || {
                self.allocatable.insert_range(slot..(slot + run))
            });
        }
        self.freeing.clear();

        (allocated_bytes, freed_bytes)
    }

    fn condense_to_spacemap(&self, spacemap: &mut SpaceMap) {
        // TODO: In the future we may want to check if writing the whole
        //       RoaringBitmap as a first-class spacemap entry is more
        //       practical here.
        let mut written_slots = 0;
        for (slot, run) in self.allocatable.iter_inverse_ranges(0, self.total_slots) {
            spacemap.alloc(Extent {
                location: self.slot_to_location(slot),
                size: u64::from(run) * u64::from(self.slot_size),
            });
            written_slots += run;
        }
        assert_eq!(written_slots, self.total_slots - self.allocatable.len());

        // In our attempt to make this independent of flush_to_spacemap(), we do not mutate any
        // of the in-memory data structures and mark all entries from the allocating bitmap as
        // free. The latter is because these entries will be later marked as allocated in
        // flush_to_spacemap().
        for (slot, run) in self.allocating.iter_ranges() {
            spacemap.free(Extent {
                location: self.slot_to_location(slot),
                size: u64::from(run) * u64::from(self.slot_size),
            });
        }
    }

    fn max_size(&self) -> u32 {
        self.slot_size
    }

    fn capacity_bytes(&self) -> u64 {
        // Compute from slot size rather than return slab size since the slot size
        // may not evenly divide the slab size, so some slab space may not be available.
        u64::from(self.total_slots) * u64::from(self.slot_size)
    }

    fn free_space(&self) -> u64 {
        u64::from(self.allocatable.len()) * u64::from(self.slot_size)
    }

    fn freeing_space(&self) -> u64 {
        u64::from(self.freeing.len()) * u64::from(self.slot_size)
    }

    fn allocated_space(&self) -> u64 {
        u64::from(self.total_slots - self.allocatable.len() - self.freeing.len())
            * u64::from(self.slot_size)
    }

    fn mark_slab_info(&self, id: SlabId, spacemap: &mut SpaceMap) {
        spacemap.mark_slab_info(
            id,
            SlabPhysType::BitmapBased {
                block_size: self.slot_size,
            },
        );
    }

    fn dump_info(&self) {
        let used_slots = self.total_slots - self.allocatable.len();
        writeln_stdout!(
            "slab_offset: {} slot_size: {} slots_used: {}/{} utilization: {:.1}%",
            self.location.offset(),
            nice_p2size(u64::from(self.slot_size)),
            used_slots,
            self.total_slots,
            (f64::from(used_slots) * 100.0) / f64::from(self.total_slots)
        );
        for (slot, run) in self.allocatable.iter_inverse_ranges(0, self.total_slots) {
            let first_location = self.slot_to_location(slot);
            let last_location = self.slot_to_location(slot + run);
            writeln_stdout!(
                "\tALLOC {:?} offset: [{}, {}) length: {} - slots: [{}, {}) count: {}",
                first_location.disk(),
                first_location.offset(),
                last_location.offset(),
                nice_p2size(last_location - first_location),
                slot,
                slot + run,
                run
            );
        }
        writeln_stdout!();
    }

    fn num_segments(&self) -> u64 {
        self.allocatable
            .iter_inverse_ranges(0, self.total_slots)
            .count() as u64
    }

    // Return a sorted list of allocated extents; each extent may cover multiple adjacent allocated
    // slots/blocks on disk.
    fn allocated_extents(&self) -> Vec<Extent> {
        let mut allocated = BitRange::new();

        self.allocatable
            .iter_inverse_ranges(0, self.total_slots)
            .for_each(|(slot, run)| {
                allocated.insert_range(slot..(slot + run));
            });

        // Due to how frees are not immediately reflected in "allocatable", we need to be careful
        // to account for them seperately, here.
        self.freeing.iter_ranges().for_each(|(slot, run)| {
            allocated.remove_range(slot..(slot + run));
        });

        allocated
            .iter_ranges()
            .map(|(slot, run)| {
                Extent::new(
                    self.location.disk(),
                    self.slot_to_location(slot).offset(),
                    u64::from(run) * u64::from(self.slot_size),
                )
            })
            .collect()
    }

    fn location(&self) -> DiskLocation {
        self.location
    }
}

struct ExtentSlab {
    allocatable: RangeTree,
    allocating: RangeTree,
    freeing: RangeTree,
    last_location: u64,

    total_space: u64,
    max_allowed_alloc_size: u32,
    location: DiskLocation,
}

impl ExtentSlab {
    const ALLOCATABLE_TAG: &'static str = "ExtentSlab.allocatable";

    fn new_slab(id: SlabId, extent: Extent, max_allowed_alloc_size: u32) -> Slab {
        let mut allocatable: RangeTree = Default::default();
        with_alloctag(Self::ALLOCATABLE_TAG, || {
            allocatable.add(extent.location.offset(), extent.size)
        });
        Slab::new(
            id,
            SlabEnum::ExtentBased(ExtentSlab {
                allocatable,
                allocating: Default::default(),
                freeing: Default::default(),
                last_location: 0,
                total_space: extent.size,
                max_allowed_alloc_size,
                location: extent.location,
            }),
        )
    }

    fn verify_slab_extent(&self, extent: Extent) {
        assert_ge!(extent.location, self.location);
        assert_le!(
            extent.location + extent.size,
            self.location + self.total_space
        );
    }

    fn allocate_impl(&mut self, size: u64, min_offset: u64, max_offset: u64) -> Option<Extent> {
        for (&allocatable_offset, &allocatable_size) in
            self.allocatable.range(min_offset..max_offset)
        {
            if allocatable_size >= size {
                self.freeing.verify_absent(allocatable_offset, size);
                with_alloctag(Self::ALLOCATABLE_TAG, || {
                    self.allocatable.remove(allocatable_offset, size)
                });
                self.allocating.add(allocatable_offset, size);
                self.last_location = allocatable_offset + size;
                return Some(Extent::new(self.location.disk(), allocatable_offset, size));
            }
        }
        None
    }

    fn slab_end(&self) -> DiskLocation {
        self.location + self.total_space
    }
}

impl SlabTrait for ExtentSlab {
    fn import_alloc(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);
        with_alloctag(Self::ALLOCATABLE_TAG, || {
            self.allocatable
                .remove(extent.location.offset(), extent.size)
        });
    }

    fn import_free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);
        with_alloctag(Self::ALLOCATABLE_TAG, || {
            self.allocatable.add(extent.location.offset(), extent.size)
        });
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        // It doesn't make any sense to do an allocation of 0 size.
        assert_ne!(size, 0);

        let request_size = u64::from(size);
        // find next segment where this fits
        match self.allocate_impl(request_size, self.last_location, u64::MAX) {
            Some(e) => Some(e),
            None => self.allocate_impl(request_size, 0, self.last_location),
        }
    }

    fn free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);

        let offset = extent.location.offset();
        let size = extent.size;

        self.allocatable.verify_absent(offset, size);
        self.freeing.add(offset, size);
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) -> (u64, u64) {
        self.freeing.verify_space();
        self.allocating.verify_space();
        self.allocatable.verify_space();

        let disk = self.location.disk();

        // It could happen that a segment was allocated and then freed within the same checkpoint
        // period at which point it would be part of both `allocating` and `freeing` sets. For
        // this reason we always record `allocating` first, before `freeing`, on our spacemaps.
        // Note that segments cannot be freed and then allocated within the same checkpoint
        // period.
        let allocated_bytes = self.allocating.space();
        for (&start, &size) in self.allocating.iter() {
            self.allocatable.verify_absent(start, size);
            spacemap.alloc(Extent::new(disk, start, size));
        }
        self.allocating.clear();

        // Space freed during this checkpoint is now available for reallocation.
        let freed_bytes = self.freeing.space();
        for (&start, &size) in self.freeing.iter() {
            self.allocating.verify_absent(start, size);
            spacemap.free(Extent::new(disk, start, size));
            with_alloctag(Self::ALLOCATABLE_TAG, || self.allocatable.add(start, size));
        }
        self.freeing.clear();

        (allocated_bytes, freed_bytes)
    }

    fn condense_to_spacemap(&self, spacemap: &mut SpaceMap) {
        let disk = self.location.disk();

        for (offset, size) in self
            .allocatable
            .iter_inverse(self.location.offset(), self.slab_end().offset())
        {
            spacemap.alloc(Extent::new(disk, offset, size));
        }

        // In our attempt to make this independent of flush_to_spacemap(), we do not mutate any
        // of the in-memory data structures and mark all entries from the allocating tree as
        // free. The latter is because these entries will be later marked as allocated in
        // flush_to_spacemap().
        for (&start, &size) in self.allocating.iter() {
            self.allocatable.verify_absent(start, size);
            spacemap.free(Extent::new(disk, start, size));
        }
    }

    fn max_size(&self) -> u32 {
        self.max_allowed_alloc_size
    }

    fn capacity_bytes(&self) -> u64 {
        self.total_space
    }

    fn free_space(&self) -> u64 {
        self.allocatable.space()
    }

    fn freeing_space(&self) -> u64 {
        self.freeing.space()
    }

    fn allocated_space(&self) -> u64 {
        self.total_space - self.free_space() - self.freeing_space()
    }

    fn mark_slab_info(&self, id: SlabId, spacemap: &mut SpaceMap) {
        spacemap.mark_slab_info(
            id,
            SlabPhysType::ExtentBased {
                max_size: self.max_allowed_alloc_size,
            },
        );
    }

    fn dump_info(&self) {
        writeln_stdout!(
            "slab_offset: {} max_allowed_alloc_size: {} allocated_bytes: {} utilization: {}%",
            self.location.offset(),
            nice_p2size(u64::from(self.max_allowed_alloc_size)),
            nice_p2size(self.total_space - self.allocatable.space()),
            ((self.total_space - self.allocatable.space()) * 100) / self.total_space
        );
        for (offset, size) in self
            .allocatable
            .iter_inverse(self.location.offset(), self.slab_end().offset())
        {
            writeln_stdout!(
                "\tALLOC offset: [{}  {}) length: {}",
                offset,
                offset + size,
                nice_p2size(size),
            );
        }
        writeln_stdout!();
    }

    fn num_segments(&self) -> u64 {
        self.allocatable
            .iter_inverse(self.location.offset(), self.slab_end().offset())
            .count() as u64
    }

    // Return a sorted list of allocated extents; each extent may cover multiple adjacent allocated
    // slots/blocks on disk.
    fn allocated_extents(&self) -> Vec<Extent> {
        let mut allocated: RangeTree = Default::default();

        self.allocatable
            .iter_inverse(self.location.offset(), self.slab_end().offset())
            .for_each(|(offset, size)| {
                allocated.add(offset, size);
            });

        // Due to how frees are not immediately reflected in "allocatable", we need to be careful
        // to account for them seperately, here.
        self.freeing.iter().for_each(|(&offset, &size)| {
            allocated.remove(offset, size);
        });

        allocated
            .iter()
            .map(|(&offset, &size)| Extent::new(self.location.disk(), offset, size))
            .collect()
    }

    fn location(&self) -> DiskLocation {
        self.location
    }
}

struct EvacuatingSlab {
    extent: Extent,
}

impl EvacuatingSlab {
    fn new_slab(id: SlabId, extent: Extent) -> Slab {
        Slab::new(id, SlabEnum::Evacuating(EvacuatingSlab { extent }))
    }
}

impl SlabTrait for EvacuatingSlab {
    fn import_alloc(&mut self, extent: Extent) {
        panic!("attempting to import alloc {:?} on evacuating slab", extent);
    }

    fn import_free(&mut self, extent: Extent) {
        panic!("attempting to import free {:?} on evacuating slab", extent);
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        panic!(
            "attempting to allocate block from evacuating slab: size = {}",
            size
        );
    }

    fn free(&mut self, extent: Extent) {
        panic!(
            "attempting to free block from evacuating slab: {:?}",
            extent
        );
    }

    fn flush_to_spacemap(&mut self, _: &mut SpaceMap) -> (u64, u64) {
        panic!("attempting to flush evacuating slab");
    }

    fn condense_to_spacemap(&self, _: &mut SpaceMap) {
        // Nothing to condense for evacuating slabs
    }

    fn max_size(&self) -> u32 {
        panic!("evacuating slab doesn't have a maximum allocation size");
    }

    fn capacity_bytes(&self) -> u64 {
        0
    }

    fn free_space(&self) -> u64 {
        0
    }

    fn freeing_space(&self) -> u64 {
        0
    }

    fn allocated_space(&self) -> u64 {
        0
    }

    fn mark_slab_info(&self, id: SlabId, spacemap: &mut SpaceMap) {
        spacemap.mark_slab_info(id, SlabPhysType::Evacuating);
    }

    fn dump_info(&self) {
        writeln_stdout!("{:?}", self.extent);
        writeln_stdout!();
    }

    fn num_segments(&self) -> u64 {
        0
    }

    fn allocated_extents(&self) -> Vec<Extent> {
        panic!("evacuating slab doesn't track allocated extents");
    }

    fn location(&self) -> DiskLocation {
        self.extent.location
    }
}

enum SlabEnum {
    BitmapBased(BitmapSlab),
    ExtentBased(ExtentSlab),
    Evacuating(EvacuatingSlab),
}

impl SlabEnum {
    fn as_dyn(&self) -> &dyn SlabTrait {
        match self {
            SlabEnum::BitmapBased(t) => t,
            SlabEnum::ExtentBased(t) => t,
            SlabEnum::Evacuating(t) => t,
        }
    }

    fn as_mut_dyn(&mut self) -> &mut dyn SlabTrait {
        match self {
            SlabEnum::BitmapBased(t) => t,
            SlabEnum::ExtentBased(t) => t,
            SlabEnum::Evacuating(t) => t,
        }
    }
}

struct Slab {
    id: SlabId,
    inner: SlabEnum,
    is_dirty: bool,
    is_allocd: bool, // used for logging
}

impl Slab {
    fn new(id: SlabId, inner: SlabEnum) -> Slab {
        Slab {
            id,
            inner,
            is_dirty: false,
            is_allocd: false,
        }
    }

    fn import_alloc(&mut self, extent: Extent) {
        self.inner.as_mut_dyn().import_alloc(extent)
    }

    fn import_free(&mut self, extent: Extent) {
        self.inner.as_mut_dyn().import_free(extent)
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        self.is_allocd = true;
        self.inner.as_mut_dyn().allocate(size)
    }

    fn free(&mut self, extent: Extent) {
        self.inner.as_mut_dyn().free(extent)
    }

    fn mark_slab_info(&self, spacemap: &mut SpaceMap) {
        self.inner.as_dyn().mark_slab_info(self.id, spacemap);
    }

    // Returns (bytes_allocated, bytes_freed)
    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) -> (u64, u64) {
        self.is_dirty = false;
        self.is_allocd = false;
        self.inner.as_mut_dyn().flush_to_spacemap(spacemap)
    }

    fn condense_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        // By leaving a new mark with the slab info when condensing we make the entries in the old
        // spacemap obsolete.
        self.mark_slab_info(spacemap);
        self.inner.as_mut_dyn().condense_to_spacemap(spacemap)
    }

    fn max_size(&self) -> u32 {
        self.inner.as_dyn().max_size()
    }

    fn free_space(&self) -> u64 {
        self.inner.as_dyn().free_space()
    }

    fn freeing_space(&self) -> u64 {
        self.inner.as_dyn().freeing_space()
    }

    fn allocated_space(&self) -> u64 {
        self.inner.as_dyn().allocated_space()
    }

    fn capacity_bytes(&self) -> u64 {
        self.inner.as_dyn().capacity_bytes()
    }

    fn num_segments(&self) -> u64 {
        self.inner.as_dyn().num_segments()
    }

    fn allocated_extents(&self) -> Vec<Extent> {
        self.inner.as_dyn().allocated_extents()
    }

    fn to_slab_bucket_entry(&self) -> SlabBucketEntry {
        SlabBucketEntry {
            allocated_space: self.allocated_space(),
            slab_id: self.id,
        }
    }

    fn dump_info(&self) {
        writeln_stdout!("{:?}", self.id);
        self.inner.as_dyn().dump_info()
    }

    fn location(&self) -> DiskLocation {
        self.inner.as_dyn().location()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SlabBucketEntry {
    allocated_space: u64,
    slab_id: SlabId,
}

struct SlabBucket {
    is_extent_based: bool,
    by_freeness: BTreeSet<SlabBucketEntry>,
    last_allocated: Option<SlabBucketEntry>,
    been_through_once: bool,
}

impl SlabBucket {
    fn new<I>(is_extent_based: bool, iter: I) -> SlabBucket
    where
        I: IntoIterator<Item = SlabBucketEntry>,
    {
        let mut by_freeness = BTreeSet::default();
        for x in iter {
            by_freeness.insert(x);
        }
        let last_allocated = by_freeness.iter().next().copied();
        SlabBucket {
            is_extent_based,
            by_freeness,
            last_allocated,
            been_through_once: false,
        }
    }

    fn get_current(&self) -> Option<SlabId> {
        self.last_allocated.map(|entry| entry.slab_id)
    }

    fn advance(&mut self) -> Option<SlabId> {
        if self.been_through_once {
            // If this clause is hit it means that we've been through all the slabs in this
            // SortedSlab set and we've also filled up a slab that we just created and inserted
            // to the set. In order to not iterate through all the slabs again for this
            // checkpoint we set last_allocated to None and return that.
            self.last_allocated = None;
        }

        if let Some(last_allocated) = self.last_allocated {
            self.last_allocated = self
                .by_freeness
                .range((Excluded(last_allocated), Unbounded))
                .next()
                .copied();
        }

        if self.last_allocated.is_none() {
            // We've iterated through all the existing slabs in the SortedSlab set for this
            // checkpoint. This flag will be reset when we re-create this SortedSlabs at the end
            // of the checkpoint.
            self.been_through_once = true;
        }
        self.get_current()
    }

    fn insert(&mut self, entry: SlabBucketEntry) {
        self.by_freeness.insert(entry);
        self.last_allocated = Some(entry);
    }

    fn remove(&mut self, id: SlabId) {
        if let Some(last) = self.last_allocated {
            // If we are removing the slab that matches the allocation cursor, we need to ensure
            // we advance the cursor so that future allocations don't attempt to allocate from
            // this removed slab.
            if last.slab_id == id {
                self.advance();
            }
        }

        let removed = self.by_freeness.remove(
            &self
                .by_freeness
                .iter()
                .find(|e| e.slab_id == id)
                .unwrap()
                .clone(),
        );
        assert!(removed);
    }
}

// key - max allocation that this set of slabs can satisfy
// value - the set of sorted slabs
//
// Note: Even though not strictly necessary, in general the BitmapBased slabs are before all the
// ExtentBased ones (i.e. Bitmaps are used for smaller allocation sizes).
struct SlabAllocationBuckets(BTreeMap<SlabBucketSize, SlabBucket>);

impl SlabAllocationBuckets {
    fn new(
        phys: SlabAllocationBucketsPhys,
        mut slabs: BTreeMap<SlabBucketSize, Vec<SlabBucketEntry>>,
    ) -> Self {
        let mut buckets = BTreeMap::new();
        for (max_size, is_extent_based) in phys.buckets {
            buckets.insert(
                max_size,
                SlabBucket::new(is_extent_based, slabs.remove(&max_size).unwrap_or_default()),
            );
        }

        // We expect for all slabs passed in to be consumed and added to a bucket.
        assert!(slabs.is_empty());

        SlabAllocationBuckets(buckets)
    }

    fn get_bucket_size_for_allocation_size(&mut self, request_size: u32) -> SlabBucketSize {
        let (bucket, _) = self
            .0
            .range_mut(SlabBucketSize(request_size)..)
            .next()
            .expect("allocation request larger than largest configured slab type");

        *bucket
    }

    fn get_bucket_for_bucket_size(&mut self, bucket_size: SlabBucketSize) -> &mut SlabBucket {
        self.0.get_mut(&bucket_size).unwrap()
    }

    fn remove_slab(&mut self, slab: &Slab) {
        let bucket_size = SlabBucketSize(slab.max_size());
        self.0.get_mut(&bucket_size).unwrap().remove(slab.id);
    }
}

pub struct BlockAllocatorBuilder {
    block_access: Arc<BlockAccess>,
    slabs: Slabs,
    phys: BlockAllocatorPhys,
}

impl BlockAllocatorBuilder {
    /// Claims the slabs used by the block allocator with the SlabAllocatorBuilder.
    pub async fn new(
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
        phys: BlockAllocatorPhys,
    ) -> Self {
        let slabs = Slabs::open(
            block_access.clone(),
            slab_access,
            &phys.spacemap,
            &phys.spacemap_next,
        )
        .await;
        Self {
            block_access,
            slabs,
            phys,
        }
    }

    pub fn claim(&self, slab_builder: &mut SlabAllocatorBuilder) {
        for slab in self.slabs.iter() {
            slab_builder.claim(slab.id);
        }
    }

    pub fn build(self, slab_allocator: Arc<SlabAllocator>) -> BlockAllocator {
        let phys = self.phys;
        let slabs = self.slabs;
        let block_access = self.block_access;
        let removing_disks = slab_allocator.removing_disks().collect::<HashSet<_>>();

        let next_slab_to_condense = phys.next_slab_to_condense;

        let spacemap = SpaceMap::open(block_access.clone(), slab_allocator.clone(), phys.spacemap);
        let spacemap_next = SpaceMap::open(
            block_access.clone(),
            slab_allocator.clone(),
            phys.spacemap_next,
        );

        let mut evacuating_slabs = Vec::new();
        let mut noalloc_state = VecMap::<_, HashSet<_>>::default();
        let mut per_disk_stats = block_access
            .disks()
            .map(|disk| (disk, DiskStats::default()))
            .collect::<VecMap<_, _>>();
        let mut slabs_by_bucket: BTreeMap<SlabBucketSize, Vec<SlabBucketEntry>> = BTreeMap::new();
        for slab in slabs.iter() {
            let slab_disk = slab.location().disk();

            let disk_stats = per_disk_stats.get_mut(slab_disk).unwrap();
            disk_stats.free_bytes += slab.free_space();
            disk_stats.alloc_bytes += slab.allocated_space();

            let evacuating = matches!(slab.inner, SlabEnum::Evacuating(_));
            if evacuating {
                evacuating_slabs.push(slab.id);
            }
            if let Some(&disk) = removing_disks.get(&slab_disk) {
                noalloc_state.get_mut_or_default(disk).insert(slab.id);
                disk_stats.noalloc_bytes += slab.free_space();
            } else if !evacuating {
                assert_eq!(disk_stats.noalloc_bytes, 0);
                slabs_by_bucket
                    .entry(SlabBucketSize(slab.max_size()))
                    .or_default()
                    .push(slab.to_slab_bucket_entry());
            }
        }

        let slab_buckets = SlabAllocationBuckets::new(phys.slab_buckets, slabs_by_bucket);
        BlockAllocator {
            slab_size: slab_allocator.slab_size().try_into().unwrap(),
            spacemap,
            spacemap_next,
            next_slab_to_condense,
            segments_at_last_merge: phys.segments_at_last_merge,
            slabs,
            dirty_slabs: Default::default(),
            slab_allocator,
            evacuating_slabs,
            noalloc_state,
            slab_buckets,
            per_disk_stats,
            checkpoint_allocated_bytes: 0,
            block_access,
        }
    }
}

#[derive(Default)]
struct DiskStats {
    // The amount of allocated space in the slabs held by the block_allocator
    alloc_bytes: u64,
    // The amount of free space in the slabs held by the block_allocator (includes noalloc_bytes)
    free_bytes: u64,
    // The number of bytes that are free in the noalloc_slabs but is not available for
    // allocations
    noalloc_bytes: u64,
}

pub struct BlockAllocator {
    slab_size: u32,

    // # Spacemap Condensing - Design Overview
    //
    // We need to condense our spacemap in order to not run out of space.
    //
    // In a scheme where one spacemap is used to log the changes from all the slabs, condensing
    // would be expensive for workloads where there are lots of incoming allocations/frees
    // because these changes would need to wait for condensing to be done before they are
    // applied.
    //
    // On the other hand, having one spacemap per slab and choosing how many of them to condense
    // dynamically based on the workload could be a viable option. Unfortunately, it comes with
    // its own set of problems too. Specifically, for big devices that have a lot of slabs with a
    // small amount of pending changes each, condensing would cause scattered I/0s whose block
    // size won't be fully utilized, affecting our overall bandwidth as a result.
    //
    // The above antithetical designs highlight a tension in the number of spacemaps we choose to
    // represent our slabs and the problems that come up if you have too many or too little of
    // them. Picking the right number of spacemaps is hard, primarily because that number is
    // workload dependend and dynamically changing it is not something that can be done in a
    // straightforward manner.
    //
    // For this block allocator we decided to approach things differently.  We use a two spacemap
    // scheme (`spacemap` and `spacemap_next`) where a certain number of slabs are condensed in a
    // round-robin fashion every checkpoint. Initially all slabs flush their changes to the first
    // spacemap (`spacemap`). Whenever a slab is condensed, we place its condensed
    // entries/representation to the second spacemap (`spacemap_next`).  Every subsequent
    // changes/flushes for that slab are also placed on that spacemap. Once we've done a full
    // circle and all slabs have been moved to `spacemap_next`, then `spacemap` is no longer
    // needed. At that point we get rid of `spacemap`, replacing it with `spacemap_next`, and use
    // an empty spacemap as `spacemap_next` for our next round of condensing.
    //
    // With the above design we use at most 2 I/Os where we expect the blocksize to be utilized
    // as the two spacemaps represent all the slabs in the Zettacache. Furthermore, we can
    // dynamically adjust the condensing rate however we see fit, making sure that our spacemaps
    // don't grow too long and that condensing itself doesn't interfere too much with other
    // activity. [see block comment above SLAB_CONDENSE_* tunables]
    spacemap: SpaceMap,
    spacemap_next: SpaceMap,
    next_slab_to_condense: SlabId,

    slabs: Slabs,
    dirty_slabs: Vec<SlabId>,
    slab_allocator: Arc<SlabAllocator>,
    evacuating_slabs: Vec<SlabId>,

    // This field contains all the data related to the removal logic. Each disk ID that is used
    // as a key in this map is a device marked for removal. The value is the set of slabs that
    // are part of that disk (which are managed by the BlockAllocator). These slabs are not in
    // the allocatable buckets, so they are not allocatable.  The removal logic uses this field
    // as follows:
    //
    // [1] When we mark a device for removal in the block allocator (see `mark_noalloc_disk()`)
    // we move any slabs that belong to that disk, away from the allocation buckets, into a new
    // disk entry in `noalloc_state` as a set (effectively marking the slabs as unavailable for
    // future allocations).
    // [2] Later, when there is enough free space in the cache for the allocated_space from those
    // noalloc slabs, the merge code will submit a remap to move that allocated space to slabs of
    // non-removing disks.
    // [3] When the remap code empties all the noalloc slabs of the removing disk and gives them
    // back to the slab_allocator then we no longer need that disk ID key and we remove it from
    // our map.
    //
    // For details about cancellation see `unmark_noalloc_disk()`.
    //
    // This field is essentially a marker. Its keys are disk IDs of devices that are being
    // removed AND have allocated space in the block allocator. So if a device marked for removal
    // has no slabs currently in-use from the block allocator, it will not be in the map.
    noalloc_state: VecMap<DiskId, HashSet<SlabId>>,

    slab_buckets: SlabAllocationBuckets,

    // Space statistics per disk
    per_disk_stats: VecMap<DiskId, DiskStats>,

    // used only by incoming rate heuristic for condensing
    checkpoint_allocated_bytes: u64,
    segments_at_last_merge: u64,

    block_access: Arc<BlockAccess>,
}

impl BlockAllocator {
    fn dirty_slab_id(&mut self, slab_id: SlabId) {
        let slab = self.slabs.get_mut(slab_id);
        if !slab.is_dirty {
            self.dirty_slabs.push(slab_id);
            slab.is_dirty = true;
        }
    }

    fn allocate_from_new_slab(&mut self, request_size: u32) -> Option<Extent> {
        let new_id = match self.slab_allocator.allocate() {
            Some(id) => id,
            None => {
                return None;
            }
        };
        let extent = self.slab_allocator.slab_id_to_extent(new_id);

        let bucket_size = self
            .slab_buckets
            .get_bucket_size_for_allocation_size(request_size);

        let bucket = self.slab_buckets.get_bucket_for_bucket_size(bucket_size);

        let mut new_slab = if bucket.is_extent_based {
            ExtentSlab::new_slab(new_id, extent, bucket_size.0)
        } else {
            BitmapSlab::new_slab(new_id, extent, bucket_size.0)
        };
        bucket.insert(new_slab.to_slab_bucket_entry());

        let extent = new_slab.allocate(request_size).unwrap();

        self.stats_add_new_slab(&new_slab);
        self.stats_track_allocation(extent);

        let old = self.slabs.insert(new_id, new_slab);
        assert!(old.is_none());

        self.mark_slab_info(new_id);
        self.dirty_slab_id(new_id);
        trace!("{new_id:?} added to {bucket_size:?}");

        Some(extent)
    }

    pub fn allocate(&mut self, request_size: u32) -> Option<Extent> {
        assert_ge!(self.slab_size, request_size);

        // Note: we assume allocation sizes are guaranteed to be aligned from the caller for now.
        self.block_access.verify_aligned(request_size);

        let bucket = self
            .slab_buckets
            .get_bucket_size_for_allocation_size(request_size);

        self.allocate_impl(bucket, request_size)
    }

    fn allocate_impl(&mut self, bucket_size: SlabBucketSize, request_size: u32) -> Option<Extent> {
        let bucket = self.slab_buckets.get_bucket_for_bucket_size(bucket_size);

        let slabs_in_bucket = bucket.by_freeness.len();

        // TODO - WIP Allocation Algorithm
        //
        // The current naive implemenation of the allocation is the following:
        // - We are iterating over the slabs of the our allocation bucket in sorted order from the
        //   slabs with the most free space to the ones with the least free space (according to
        //   their free space accounting since our latest flush/checkpoint).
        // - We are looking at the current slab used since our last allocation (or the first slab if
        //   this is the first allocation since the last checkpoint), and try to allocate from that.
        // - If the allocation fails we move to the next slab in our set of sorted slabs, and try to
        //   allocate from that one.
        // - If that fails too, we keep trying through all the slabs in that set until we go through
        //   them all at which point we will try to convert a FreeSlab to this type, add it to the
        //   set, and allocate from it.
        // - If that fails too then we fail the allocation (and any allocation for that allocation
        //   size until the next flush/checkpoint).
        //
        // Obviously this is far from ideal but it is deterministic and easy
        // to reason about for now.
        //
        loop {
            match bucket.get_current() {
                Some(id) => match self.slabs.get_mut(id).allocate(request_size) {
                    Some(extent) => {
                        super_trace!(
                            "satisfied {} byte allocation request: {:?}",
                            request_size,
                            extent
                        );
                        self.dirty_slab_id(id);
                        self.stats_track_allocation(extent);
                        return Some(extent);
                    }
                    None => {
                        let debug = bucket.advance();
                        super_trace!(
                            "advance slab bucket {:?} cursor to {:?}",
                            bucket_size,
                            debug
                        );
                    }
                },
                None => match self.allocate_from_new_slab(request_size) {
                    Some(extent) => {
                        super_trace!(
                            "satisfied {} byte allocation request: {:?}",
                            request_size,
                            extent
                        );
                        return Some(extent);
                    }
                    None => {
                        super_trace!(
                            "allocation of {} bytes failed; no free slabs left; {} slabs used for {:?}",
                            request_size,
                            slabs_in_bucket,
                            bucket_size
                        );
                        return None;
                    }
                },
            }
        }
    }

    pub fn free(&mut self, extent: Extent) {
        self.block_access.verify_aligned(extent.location.offset());
        self.block_access.verify_aligned(extent.size);
        super_trace!("free request: {:?}", extent);

        let slab_id = self.slab_allocator.extent_to_slab_id(extent);
        self.slabs.get_mut(slab_id).free(extent);

        self.stats_track_free(extent);
        self.dirty_slab_id(slab_id);
    }

    pub fn add_disk(&mut self, disk: DiskId) {
        let inserted = self.per_disk_stats.insert(disk, Default::default());
        assert!(inserted.is_none());
    }

    pub fn remove_disk(&mut self, disk: DiskId) {
        self.stats_verify_disk_is_empty(disk);
        let removed = self.per_disk_stats.remove(disk);
        assert!(removed.is_some());
        assert!(self.noalloc_state.get(disk).is_none());
    }

    /// Remove all slabs that belong to `disk` from our sorted slab buckets,
    /// effectively forbidding any future allocations from them.
    pub fn mark_noalloc_disk(&mut self, disk: DiskId) {
        let disk_stats = self.per_disk_stats.get_mut(disk).unwrap();
        for bucket in self.slab_buckets.0.values_mut() {
            bucket.by_freeness.retain(|entry| {
                let slab = self.slabs.get(entry.slab_id);
                if slab.location().disk() != disk {
                    true
                } else {
                    self.noalloc_state
                        .get_mut_or_default(disk)
                        .insert(entry.slab_id);

                    disk_stats.noalloc_bytes += slab.free_space();
                    false
                }
            });

            // Even if we removed all the disk's slabs from the allocation bucket the bucket can
            // still point to one of those slabs if we allocated from it recently.
            if let Some(bucket_current) = bucket.last_allocated {
                if let Some(noalloc_slabs) = self.noalloc_state.get(disk) {
                    if noalloc_slabs.contains(&bucket_current.slab_id) {
                        bucket.advance();
                    }
                }
            }
        }

        // Make sure we also track any slabs from that disk that have already been submitted for
        // evacuation.
        for &slab_id in self.evacuating_slabs.iter() {
            if self.slabs.get(slab_id).location().disk() == disk {
                self.noalloc_state.get_mut_or_default(disk).insert(slab_id);
            }
        }

        // If the supplied `disk` had any free data the space of that data should equal the
        // amount of space marked as non-allocatable.
        assert_eq!(disk_stats.free_bytes, disk_stats.noalloc_bytes);
    }

    /// Place any slabs marked as non-allocatable back to the sorted slab
    /// buckets allowing us to allocate from them again.
    pub fn unmark_noalloc_disk(&mut self, disk: DiskId) {
        // Remaping the disk's data may have already finished, at which point there is nothing
        // for us to do here.
        if !self.disk_is_pending_remap(disk) {
            self.stats_verify_disk_is_empty(disk);
            return;
        }

        let disk_stats = self.per_disk_stats.get_mut(disk).unwrap();
        self.noalloc_state
            .get_mut(disk)
            .unwrap()
            .retain(|&slab_id| {
                let slab = self.slabs.get(slab_id);
                disk_stats.noalloc_bytes -= slab.free_space();

                // Note: Any noalloc slabs that are marked as evacuating (e.g. they are currently
                // being remapped) are not part of the allocation buckets and their free space
                // is not accounted in the block allocator. Given that plus the fact that they
                // will be released back to the slab allocator once remap is done, we want to
                // skip them.
                if matches!(slab.inner, SlabEnum::Evacuating(_)) {
                    return true;
                }

                let bucket_size = self
                    .slab_buckets
                    .get_bucket_size_for_allocation_size(slab.max_size());
                self.slab_buckets
                    .get_bucket_for_bucket_size(bucket_size)
                    .insert(slab.to_slab_bucket_entry());

                false
            });
        // At this point everything is either back to allocatable or is currently evacuating.
        // Either way there noalloc_bytes should be zero.
        assert_eq!(disk_stats.noalloc_bytes, 0);

        // If the remap for this disk was never submitted, it's set of noalloc slabs should be
        // empty, in which case we can finish cleaning up its noalloc_state entry here.
        if self.noalloc_state.get(disk).unwrap().is_empty() {
            self.noalloc_state.remove(disk);
        }
    }

    fn noalloc_slabs_to_remap(&self, disk: DiskId) -> Vec<SlabId> {
        match self.noalloc_state.get(disk) {
            Some(slabs) => slabs.iter().copied().collect(),
            None => Vec::new(),
        }
    }

    /// Returns true if the disk is marked for removal and it is waiting for a remap to empty its
    /// slabs from the block allocator. Otherwise, the device is either non-removing or it is
    /// removing but has no slabs in-use from the block allocator.
    pub fn disk_is_pending_remap(&self, disk: DiskId) -> bool {
        self.noalloc_state.contains_key(&disk)
    }

    // This function is the entry-point to starting the cache rebalancing process. This will select
    // which slab(s) need to be rebalanced, mark those slabs as undergoing evacuation, and
    // allocate new disk locations for the data currently stored on those slabs. The value
    // returned by this function is a map of key-value pairs, where the key denotes the current
    // location on disk (i.e. on the slab(s) being evacuated), and the value is
    // the newly allocated disk location where the data should be moved (to facilitate the
    // evacuation).
    //
    // It is the responsibility of the consumer of this function to actually move the data from the
    // old location, to the new location. This way, the allocator remains responsible only for
    // the allocation of disk extents, and not for the reading and writing of the block data.
    //
    // Once the consumer has finished the process of moving the data blocks to their new locations,
    // it is also the consumer's responsibility to call the "rebalance_fini" function, marking
    // the rebalance process as finished. This allows the allocator to transition the slabs that
    // were undergoing evacuation to free slabs, such that the slabs can later be used for
    // allocation.
    pub fn rebalance_init(
        &mut self,
        removing_disk: Option<DiskId>,
    ) -> Option<BTreeMap<Extent, Option<DiskLocation>>> {
        // For now, ensure rebalance_fini() is called before this function can be called a second
        // time.
        assert!(self.evacuating_slabs.is_empty());

        let begin = Instant::now();

        let slabs = match removing_disk {
            Some(disk) => self.noalloc_slabs_to_remap(disk),
            None => self.slabs_to_rebalance(),
        };
        if slabs.is_empty() {
            info!("cache rebalance is not needed");
            return None;
        }

        info!(
            "initializing rebalance of {} slabs {}",
            slabs.len(),
            if removing_disk.is_some() {
                "(triggered by removal)"
            } else {
                ""
            }
        );

        if removing_disk.is_none() {
            // In order to ensure the allocations performed in rebalance_slab() (called below) are
            // not satisfied by any of the slabs we're going to rebalance, we need to remove these
            // slabs from the list of slabs available for allocation. Further, we must remove all
            // slabs before we do any allocations, to ensure we don't move an extent multiple times;
            // otherwise, data corruption could occur, as the data contained in the extents, can be
            // moved by the caller in any order.
            //
            // For example, if we mark an extent as moving from disk location A to B, and then again
            // from B to C, the final data contained at disk location C could be incorrect, if the
            // caller does the move of B to C before the move of A to B. Since we do not enforce the
            // order in which the caller will do the copies, we need to ensure this cannot happen,
            // by never moving an extent more than once.
            //
            // Note that for removal we've already removed the removing disk's slabs from allocation
            // buckets so there should be nothing to remove.
            for &id in slabs.iter() {
                trace!("prepping slab '{:?}' for rebalancing", id);
                self.slab_buckets.remove_slab(self.slabs.get(id));
            }
        }

        let mut merged = 0;
        let map: BTreeMap<Extent, Option<DiskLocation>> = slabs
            .iter()
            .flat_map(|&id| {
                let (entries, count) = self.rebalance_slab(id);
                merged += count;

                entries
            })
            .map(|(old, new)| (old, new.map(|extent| extent.location)))
            .collect();

        info!(
            "took {}ms to initialize rebalance of {} slabs ({} entries, {} merged)",
            begin.elapsed().as_millis(),
            slabs.len(),
            map.len(),
            merged,
        );

        if let Some(disk) = removing_disk {
            self.stats_verify_disk_is_empty(disk);
        }

        assert!(!self.evacuating_slabs.is_empty());
        Some(map)
    }

    fn slabs_to_rebalance(&self) -> Vec<SlabId> {
        let num_slabs_to_rebalance = self.slab_allocator.num_slabs_to_evacuate();

        if num_slabs_to_rebalance == 0 {
            return vec![];
        }

        trace!(
            "attempting to find {} slabs to rebalance",
            num_slabs_to_rebalance
        );

        let mut free_space_per_bucket: BTreeMap<SlabBucketSize, u64> = self
            .slab_buckets
            .0
            .iter()
            .map(|(bucket, slabs)| {
                (
                    *bucket,
                    slabs
                        .by_freeness
                        .iter()
                        .map(|entry| self.slabs.get(entry.slab_id).free_space())
                        .sum(),
                )
            })
            .collect();

        // list of slabs, sorted by free space; most free first.
        let slabs: BTreeSet<SlabBucketEntry> = self
            .slabs
            .iter()
            .filter(|&slab| match slab.inner {
                SlabEnum::BitmapBased(_) | SlabEnum::ExtentBased(_) => {
                    // Skip over slabs from disks being removed
                    !self.noalloc_state.contains_key(&slab.location().disk())
                }
                SlabEnum::Evacuating(_) => false,
            })
            .map(|slab| slab.to_slab_bucket_entry())
            .collect();

        // The goal of the rebalance, is to generate more free slabs, such that we can satisfy
        // future allocations. It doesn't matter which bucket the free slab came from; if the
        // free slab comes from a very fragmented bucket, or a very compact bucket, it doesn't
        // really matter. The only thing that matters, is that we have free slabs available, such
        // that future allocations do not fail.
        //
        // Further, a secondary goal, is to accomplish the aformentioned primary goal, but while
        // minimizing the cost of doing so; i.e. minimizing the bytes read and written by the
        // rebalacing process.
        //
        // As such, we select the slabs that we intend to rebalance, by seeking to rebalance the
        // most free slabs first. This way, we will choose the slabs that can be evacuated with
        // the least about of data transfer (i.e. disk reads and writes), regardless of the
        // bucket the slab belongs too.
        slabs
            .iter()
            .filter_map(|entry| {
                let slab = self.slabs.get(entry.slab_id);

                let bucket = SlabBucketSize(slab.max_size());
                let bytes_free_in_bucket = free_space_per_bucket.get_mut(&bucket).unwrap();

                // If there's not enough free space in the bucket to completely evacuate this
                // slab's allocated bytes, then we skip it, and move on to the next slab in the
                // (sorted) list. This way, we don't have to handle allocation failures when
                // rebalance_slab() is called.
                // XXX we should rebalance/evacuate anyway, even if it causes allocation failures.
                *bytes_free_in_bucket = bytes_free_in_bucket.checked_sub(slab.capacity_bytes())?;

                Some(slab.id)
            })
            .take(usize::try_from(num_slabs_to_rebalance).unwrap())
            .collect()
    }

    // Returns the mapping of old to new, and the number of merged extents.
    fn rebalance_slab(&mut self, id: SlabId) -> (Vec<(Extent, Option<Extent>)>, usize) {
        trace!("starting rebalance of slab '{:?}'", id);

        let slab = self.slabs.get(id);
        let bucket = self
            .slab_buckets
            .get_bucket_size_for_allocation_size(slab.max_size());

        let extents = slab
            .allocated_extents()
            .into_iter()
            .flat_map(|old| {
                match slab.inner {
                    SlabEnum::BitmapBased(_) => {
                        let extent_size = u32::try_from(old.size).unwrap();
                        let slot_size = slab.max_size();
                        assert_eq!(extent_size % slot_size, 0);

                        // For bitmap based slabs, we know the boundaries of each allocation,
                        // since each allocation must have been done in a slot-sized chuck. Thus,
                        // we can break up a multi-slot allocated extent into single-slot
                        // extents, which is what we're doing here. We choose to do this, so that
                        // when we later allocate the new location for these extents, we'll
                        // allocate in slot-sized chunks, ensuring we fill all holes in the slabs
                        // we're allocating from. Otherwise, we would have to (potentially)
                        // allocate in multi-slot contiguous chunks, and due to slab
                        // fragmentation, the slabs may not be able to fulfill those requests.
                        Either::Left(
                            (0..(extent_size / slot_size)).map(move |slot_index| Extent {
                                size: u64::from(slot_size),
                                location: old.location + u64::from(slot_index * slot_size),
                            }),
                        )
                    }
                    SlabEnum::ExtentBased(_) => Either::Right(std::iter::once(old)),
                    SlabEnum::Evacuating(_) => panic!("invalid slab type"),
                }
            })
            .collect::<Vec<_>>();

        let mut map = extents
            .iter()
            .map(
                |&old| match self.allocate_impl(bucket, u32::try_from(old.size).unwrap()) {
                    Some(new) => (old, Some(new)),
                    None => {
                        trace!("cache rebalance allocation failed for old {old:?} in {bucket:?}");
                        (old, None)
                    }
                },
            )
            .collect::<Vec<_>>();

        let before = map.len();
        trace!("rebalance of slab {id:?} has {before} entries before merging");

        // In an attempt to reduce the IO cost of a rebalance event, we try to combine any
        // contiguous entries here, by (when appropriate) combining entries with their neighbor.
        map.dedup_by(
            |(a_old, a_new), (b_old, b_new)| match (a_new, b_new, b_old.merge(*a_old)) {
                (None, None, Some(merged_old)) => {
                    *b_old = merged_old;
                    true
                }
                (Some(a_new), Some(b_new), Some(merged_old)) => {
                    if let Some(merged_new) = a_new.merge(*b_new) {
                        *b_old = merged_old;
                        *b_new = merged_new;
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            },
        );

        let after = map.len();
        let merged = before - after;
        if before != after {
            trace!(
                "rebalance of {id:?} has {after} entries after merging ({merged} entries merged)",
            );
        }

        // Since evacuating slabs don't have any allocatable space, we must account for that
        // here; we must do this before we transition to an evacuating slab (evacuating slabs
        // have no free space).
        self.stats_track_slab_evacuation(id);

        // XXX: Currently it is not possible to call `BlockAllocator.free()` from the
        // checkpoint_task in-between the point that we flush a checkpoint and the point that we
        // start a rebalance_init(). Thet is expected to change once DLPX-80824 lands.
        assert_eq!(self.slabs.get(id).freeing_space(), 0);

        trace!("marking slab '{:?}' as evacuating", id);

        self.evacuating_slabs.push(id);
        let old = self.slabs.insert(
            id,
            EvacuatingSlab::new_slab(id, self.slab_allocator.slab_id_to_extent(id)),
        );
        assert!(old.is_some());
        self.mark_slab_info(id);

        (map, merged)
    }

    // See comment above rebalance_init() for more details.
    pub fn rebalance_fini(&mut self) {
        for id in mem::take(&mut self.evacuating_slabs) {
            trace!("marking slab '{:?}' as free", id);

            // evacuating slabs cannot allocate() or free(); thus, they should never be dirty.
            assert!(!self.slabs.get(id).is_dirty);

            // if slab is part of removing disk, remove it from the noalloc slabs.
            let slab_disk = self.slabs.get(id).location().disk();
            if let Some(noalloc_set) = self.noalloc_state.get_mut(slab_disk) {
                let removed = noalloc_set.remove(&id);
                assert!(removed);
            }

            self.slab_allocator.free(id);
            self.slabs.remove(id);

            // Note: we can't use BlockAllocator::mark_slab_info, because the Evacuating slab's
            // SlabPhysType is Evacuating, and there's no Free slab.
            let target_spacemap = if self.next_slab_to_condense <= id {
                &mut self.spacemap
            } else {
                &mut self.spacemap_next
            };
            target_spacemap.mark_slab_info(id, SlabPhysType::Free);
        }

        // This could be the end of the remap of a removing disk. If that's the case the set of
        // (noalloc) slabs for that disk should be empty, which means we can remove its entry
        // from the noalloc_state.
        self.noalloc_state.retain(|_, slabs| !slabs.is_empty());
    }

    /// Return number of slabs to condense, based on the "spacemap badness" ratio.  Note that the
    /// largest of the 3 factors will be selected by condense().  See comment near
    /// SLAB_CONDENSE_MAX_BADNESS_RATIO for details.
    fn spacemap_badness_heuristic(&self) -> usize {
        // If there's less than a million entries (~10MB on disk), it isn't that bad according to
        // this metric.
        if self.spacemap.total_entries() < *SLAB_CONDENSE_MIN_BADNESS_ENTRIES {
            return 0;
        }
        let ratio = self.spacemap.total_entries() as f64 / self.segments_at_last_merge as f64;

        const MIN_RATIO: f64 = 1.0;
        let fraction = if ratio <= MIN_RATIO {
            0.0
        } else {
            (ratio - MIN_RATIO) / (*SLAB_CONDENSE_MAX_BADNESS_RATIO - MIN_RATIO)
        };
        let slabs_to_condense = (self.slabs.len() as f64 * fraction).to_usize().unwrap();
        debug!(
            "{} segs at last merge, {} old sm ents, {:.2}x ratio, {} to condense ({:.1}%)",
            nice_number_count(self.segments_at_last_merge as f64),
            nice_number_count(self.spacemap.total_entries() as f64),
            ratio,
            slabs_to_condense,
            fraction * 100.0,
        );
        slabs_to_condense
    }

    /// Return number of slabs to condense, based on the "incoming rate".  Note that the largest
    /// of the 3 factors will be selected by condense().  See comment near
    /// SLAB_CONDENSE_RATE_FACTOR for details.
    fn incoming_rate_heuristic(&self) -> usize {
        let incoming_rate_heuristic = (*SLAB_CONDENSE_RATE_FACTOR
            * (self.checkpoint_allocated_bytes as f64 / f64::from(self.slab_size)))
        .ceil()
        .to_usize()
        .unwrap();
        debug!(
            "incoming rate heuristic: {} allocated -> {} slabs",
            nice_p2size(self.checkpoint_allocated_bytes),
            incoming_rate_heuristic,
        );
        incoming_rate_heuristic
    }

    fn condense(&mut self) {
        let begin = Instant::now();
        let old_pending = self.spacemap_next.pending_len();
        let starting_slab = self.next_slab_to_condense;

        let slabs_to_condense = max(
            *SLAB_CONDENSE_MIN_PER_CHECKPOINT,
            max(
                self.incoming_rate_heuristic(),
                self.spacemap_badness_heuristic(),
            ),
        );

        let mut condensed = 0;
        for slab in self
            .slabs
            .range_mut(self.next_slab_to_condense..)
            .take(slabs_to_condense)
        {
            slab.condense_to_spacemap(&mut self.spacemap_next);
            self.next_slab_to_condense = slab.id.next();
            condensed += 1;
        }

        debug!(
            "condensed {condensed} slabs, {} entries starting from {starting_slab:?} in {}ms",
            nice_number_count((self.spacemap_next.pending_len() - old_pending) as f64),
            begin.elapsed().as_millis(),
        );

        if condensed < slabs_to_condense {
            info!(
                "finished condensing all {} slabs; deleting old spacemap ({}, {} entries)",
                self.slabs.len(),
                nice_p2size(self.spacemap.bytes()),
                nice_number_count(self.spacemap.total_entries() as f64),
            );
            self.next_slab_to_condense = SlabId(0);
            self.spacemap.clear();
            mem::swap(&mut self.spacemap_next, &mut self.spacemap);
        }
    }

    /// Flush any dirty slabs.
    fn flush_dirty(&mut self) {
        let begin = Instant::now();
        let old_pending = self.spacemap.pending_len() + self.spacemap_next.pending_len();
        let ndirty_slabs = self.dirty_slabs.len();
        let mut allocd_slabs = 0u64;
        let mut total_allocated = 0;
        let mut total_freed = 0;
        for slab_id in mem::take(&mut self.dirty_slabs) {
            if !self.slabs.exists(slab_id) {
                // This can happen if the slab was evacuated.
                continue;
            }
            let slab = self.slabs.get_mut(slab_id);

            // It's possible for a slab in the dirty list, to be converted to a different slab
            // type, such that the actual slab object is no longer dirty, but the slab's id is
            // still in the dirty list. For example, if an already dirtied slab is chosen to be
            // rebalanced.  Thus, prior to flushing the slab, we double check that the slab is
            // still dirty.
            if !slab.is_dirty {
                continue;
            }

            if slab.is_allocd {
                allocd_slabs += 1;
            }

            let target_spacemap = if self.next_slab_to_condense <= slab_id {
                &mut self.spacemap
            } else {
                &mut self.spacemap_next
            };
            let (allocated, freed) = slab.flush_to_spacemap(target_spacemap);

            self.stats_flush_freeing(slab_id, freed);
            total_allocated += allocated;
            total_freed += freed;
        }
        debug!(
            "flushed {} entries ({} allocd, {} freed) to {ndirty_slabs} ({allocd_slabs} allocated from) in {}ms",
            nice_number_count(
                (self.spacemap.pending_len() + self.spacemap_next.pending_len() - old_pending)
                    as f64
            ),
            nice_p2size(total_allocated),
            nice_p2size(total_freed),
            begin.elapsed().as_millis()
        );
    }

    /// Update all buckets by recreating their SortedSlabs (which in turn updates their order
    /// by freeness and also removes any empty slabs).
    fn resort_buckets(&mut self) {
        let begin = Instant::now();
        for bucket in self.slab_buckets.0.values_mut() {
            let iter = bucket
                .by_freeness
                .iter()
                .map(|entry| self.slabs.get(entry.slab_id).to_slab_bucket_entry());
            *bucket = SlabBucket::new(bucket.is_extent_based, iter);
        }
        debug!("resorted buckets in {}ms", begin.elapsed().as_millis());
    }

    /// returns (old_spacemap, spacemap_next)
    async fn flush_impl(&mut self) -> (SpaceMapPhys, SpaceMapPhys) {
        let begin = Instant::now();
        let old_space = self.spacemap.bytes() + self.spacemap_next.bytes();
        let pending = self.spacemap.pending_len() + self.spacemap_next.pending_len();
        let (spacemap, spacemap_next) =
            futures::future::join(self.spacemap.flush(), self.spacemap_next.flush()).await;
        debug!(
            "wrote {}, {} entries to spacemaps in {}ms [sm: {}, {}% alloc, {}] [next: {}, {}% alloc, {}]",
            nice_p2size(self.spacemap.bytes() + self.spacemap_next.bytes() - old_space),
            nice_number_count(pending as f64),
            begin.elapsed().as_millis(),
            nice_number_count(self.spacemap.total_entries() as f64),
            self.spacemap.alloc_entries() * 100 / (self.spacemap.total_entries() + 1),
            nice_p2size(self.spacemap.bytes()),
            nice_number_count(self.spacemap_next.total_entries() as f64),
            self.spacemap_next.alloc_entries() * 100 / (self.spacemap_next.total_entries() + 1),
            nice_p2size(self.spacemap_next.bytes()),
        );
        (spacemap, spacemap_next)
    }

    pub async fn flush(&mut self, completed_merge: bool) -> BlockAllocatorPhys {
        let begin = Instant::now();

        // We first condense any slabs so later when we flush any of them that are dirty we've
        // already migrated their entries of this checkpoint to spacemap_next.
        self.condense();
        self.flush_dirty();
        let (spacemap, spacemap_next) = self.flush_impl().await;
        self.resort_buckets();
        self.checkpoint_allocated_bytes = 0;

        if completed_merge {
            let begin = Instant::now();
            self.segments_at_last_merge = self.slabs.total_segments();
            info!(
                "merge frees completed; computed {} total segments (1/{}) in {}ms",
                nice_number_count(self.segments_at_last_merge as f64),
                nice_p2size(
                    (self.slab_allocator.capacity()
                        - self.slab_allocator.free_slabs() * self.slab_allocator.slab_size())
                        / (self.segments_at_last_merge + 1) // +1 to avoid divide-by-zero
                ),
                begin.elapsed().as_millis(),
            )
        }

        let phys_begin = Instant::now();
        let phys = BlockAllocatorPhys {
            spacemap,
            spacemap_next,
            segments_at_last_merge: self.segments_at_last_merge,
            next_slab_to_condense: self.next_slab_to_condense,
            slab_buckets: SlabAllocationBucketsPhys {
                buckets: self
                    .slab_buckets
                    .0
                    .iter()
                    .map(|(&bucket_size, bucket)| (bucket_size, bucket.is_extent_based))
                    .collect(),
            },
        };
        trace!(
            "computed new BlockAllocatorPhys in {}ms",
            phys_begin.elapsed().as_millis()
        );
        debug!(
            "flushed BlockAllocator in {}ms",
            begin.elapsed().as_millis()
        );
        phys
    }

    /// Return the amount of allocated space in slabs that are marked for removal.
    pub fn removing_bytes(&self) -> u64 {
        self.noalloc_state
            .keys()
            .map(|disk| self.per_disk_stats.get(disk).unwrap().alloc_bytes)
            .sum()
    }

    pub fn disk_allocated_bytes(&self, disk: DiskId) -> u64 {
        self.per_disk_stats.get(disk).unwrap().alloc_bytes
    }

    /// Return the amount of space in unallocated blocks.  This does not include
    /// space in empty slabs.
    pub fn free_bytes(&self) -> u64 {
        self.per_disk_stats
            .values()
            .map(|stats| stats.free_bytes)
            .sum::<u64>()
    }

    /// Return the amount of space in unallocated blocks that's available for
    /// allocations.  This does not include space in empty slabs nor space in
    /// slabs that are part of a device being removed.
    pub fn allocatable_bytes(&self) -> u64 {
        let free_bytes = self
            .per_disk_stats
            .values()
            .map(|stats| stats.free_bytes)
            .sum::<u64>();
        let noalloc_bytes = self
            .per_disk_stats
            .values()
            .map(|stats| stats.noalloc_bytes)
            .sum::<u64>();
        free_bytes - noalloc_bytes
    }

    /// Incorporate this newly-created slab's free space into the BlockAllocator's `per_disk_stats`.
    fn stats_add_new_slab(&mut self, slab: &Slab) {
        self.per_disk_stats
            .get_mut(slab.location().disk())
            .unwrap()
            .free_bytes += slab.capacity_bytes();
    }

    /// Track the newly-allocated `extent` in the BlockAllocator's `per_disk_stats`.
    fn stats_track_allocation(&mut self, extent: Extent) {
        let disk_stats = self.per_disk_stats.get_mut(extent.location.disk()).unwrap();
        disk_stats.free_bytes -= extent.size;
        disk_stats.alloc_bytes += extent.size;
        assert_eq!(
            disk_stats.noalloc_bytes, 0,
            "can't allocate from non-allocatable disk"
        );
        self.checkpoint_allocated_bytes += extent.size;
    }

    /// Track the `extent` that was just freed in the BlockAllocator's `per_disk_stats`.
    fn stats_track_free(&mut self, extent: Extent) {
        let disk = extent.location.disk();

        let disk_stats = self.per_disk_stats.get_mut(disk).unwrap();
        disk_stats.alloc_bytes -= extent.size;
        if !self.noalloc_state.contains_key(&disk) {
            assert_eq!(disk_stats.noalloc_bytes, 0);
        }
    }

    /// Update the `free_bytes` of the disk where `slab_id` belongs to with the amount that was
    /// just `freed` when flushing that slab. If that slab is part of a removing disk then
    /// `noalloc_bytes` from that disk is also incremented by `freed`.
    fn stats_flush_freeing(&mut self, slab_id: SlabId, freed: u64) {
        let disk = self.slabs.get(slab_id).location().disk();

        let disk_stats = self.per_disk_stats.get_mut(disk).unwrap();
        disk_stats.free_bytes += freed;
        if self.noalloc_state.contains_key(&disk) {
            disk_stats.noalloc_bytes += freed;
        }
    }

    /// Remove any space accounted for the supplied slab from `per_disk_stats` as we are
    /// preparing it for evacuation.
    fn stats_track_slab_evacuation(&mut self, slab_id: SlabId) {
        let slab = self.slabs.get(slab_id);
        let slab_disk = slab.location().disk();

        let disk_stats = self.per_disk_stats.get_mut(slab_disk).unwrap();
        disk_stats.free_bytes -= slab.free_space();
        disk_stats.alloc_bytes -= slab.allocated_space();
        if self.noalloc_state.contains_key(&slab_disk) {
            disk_stats.noalloc_bytes -= slab.free_space();
        } else {
            assert_eq!(disk_stats.noalloc_bytes, 0);
        }
    }

    /// Verify that the supplied `disk` doesn't contribute any space in `per_disk_stats`.
    fn stats_verify_disk_is_empty(&self, disk: DiskId) {
        let disk_stats = self.per_disk_stats.get(disk).unwrap();
        assert_eq!(disk_stats.alloc_bytes, 0);
        assert_eq!(disk_stats.free_bytes, 0);
        assert_eq!(disk_stats.noalloc_bytes, 0);
    }

    fn mark_slab_info(&mut self, id: SlabId) {
        let slab = self.slabs.get(id);
        let target_spacemap = if self.next_slab_to_condense <= id {
            &mut self.spacemap
        } else {
            &mut self.spacemap_next
        };
        slab.mark_slab_info(target_spacemap);
    }

    /// Returns the number of slabs moved.
    pub async fn transfer_metadata_for_removal(&mut self, disk: DiskId) -> u64 {
        let mut moved = self.spacemap.transfer_data_for_removal(disk).await;
        moved += self.spacemap_next.transfer_data_for_removal(disk).await;
        moved
    }

    /// Returns the amount of space that needs to be removed before a removing device has been
    /// fully evacuated.
    pub fn disk_space_to_evacuate(&self, disk: DiskId) -> u64 {
        let block_allocator_slabs = self
            .noalloc_state
            .get(disk)
            .map(|slabs| slabs.len() as u64)
            .unwrap_or_default();
        let slab_allocator_slabs = self.slab_allocator.disk_slabs_to_evacuate(disk);
        let metadata_slabs = slab_allocator_slabs
            .checked_sub(block_allocator_slabs)
            .unwrap();

        // The slab allocator is only aware of the slabs in use by the block allocator and
        // metadata (like BBLs) but doesn't have knowledge of their internal allocation state.
        // We have counters for per disk allocation state for the slabs used in the block
        // allocator but nothing for our metadata.  In the calculation below we assume that
        // metadata slabs are fully allocated which should be true in general for big structures
        // like the index where all but the last slab in each BBL are fully allocated.
        self.per_disk_stats.get(disk).unwrap().alloc_bytes
            + (metadata_slabs * self.slab_allocator.slab_size())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub enum SlabPhysType {
    BitmapBased { block_size: u32 },
    ExtentBased { max_size: u32 },
    Free,
    Evacuating,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SlabAllocationBucketsPhys {
    // Buckets sorted by max-allocation-size (ascending-order)
    // (max allocation size, is extent based)
    buckets: Vec<(SlabBucketSize, bool)>,
}

impl SlabAllocationBucketsPhys {
    fn default() -> Self {
        let mut buckets = Vec::new();
        // Create the first few buckets for bitmap-based slab use
        for b in 1..(16 * 1024 / 512) {
            buckets.push((SlabBucketSize(b * 512), false));
        }
        // Create a few more extent-based buckets for larger sizes
        buckets.push((SlabBucketSize(64 * 1024), true));
        buckets.push((SlabBucketSize(256 * 1024), true));
        buckets.push((SlabBucketSize(1024 * 1024), true));
        buckets.push((
            SlabBucketSize(DEFAULT_SLAB_SIZE.as_u64().try_into().unwrap()),
            true,
        ));

        SlabAllocationBucketsPhys { buckets }
    }
}

#[derive(Derivative, Serialize, Deserialize, Clone)]
#[derivative(Debug)]
pub struct BlockAllocatorPhys {
    spacemap: SpaceMapPhys,
    spacemap_next: SpaceMapPhys,
    next_slab_to_condense: SlabId,
    segments_at_last_merge: u64,

    slab_buckets: SlabAllocationBucketsPhys,
}

impl BlockAllocatorPhys {
    pub fn new(block_access: &BlockAccess) -> BlockAllocatorPhys {
        let mut bucket_sizes = HashSet::new();
        let mut buckets = Vec::new();
        for (default_bucket, is_extent_based) in &DEFAULT_SLAB_BUCKETS.buckets {
            let aligned_bucket = SlabBucketSize(block_access.round_up_to_sector(default_bucket.0));
            if !bucket_sizes.contains(&aligned_bucket) {
                assert_le!(
                    aligned_bucket.0,
                    DEFAULT_SLAB_SIZE.as_u64().try_into().unwrap()
                );
                bucket_sizes.insert(aligned_bucket);
                buckets.push((aligned_bucket, *is_extent_based));
            }
        }
        BlockAllocatorPhys {
            spacemap: SpaceMapPhys::new(),
            spacemap_next: SpaceMapPhys::new(),
            next_slab_to_condense: SlabId(0),
            segments_at_last_merge: 0,
            slab_buckets: SlabAllocationBucketsPhys { buckets },
        }
    }

    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.spacemap.claim(builder);
        self.spacemap_next.claim(builder);
    }

    pub fn spacemap_bytes(&self) -> u64 {
        self.spacemap.bytes()
    }

    pub fn spacemap_next_bytes(&self) -> u64 {
        self.spacemap_next.bytes()
    }
}
