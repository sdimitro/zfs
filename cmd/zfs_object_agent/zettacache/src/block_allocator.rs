use crate::block_access::BlockAccess;
use crate::extent_allocator::{ExtentAllocator, ExtentAllocatorBuilder};
use crate::space_map::{SpaceMap, SpaceMapEntry, SpaceMapPhys};
use crate::{base_types::*, DumpSlabsOptions};
use bimap::BiBTreeMap;
use lazy_static::lazy_static;
use log::*;
use more_asserts::*;
use rand::seq::SliceRandom;
use rand::thread_rng;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::cmp::{self, min};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::{Add, Bound::*, Sub};
use std::sync::Arc;
use std::time::Instant;
use std::{fmt, iter, mem};
use util::BitmapRangeIterator;
use util::RangeTree;
use util::{get_tunable, TerseVec};
use util::{nice_p2size, From64};

lazy_static! {
    static ref DEFAULT_SLAB_SIZE: u32 = get_tunable("default_slab_size", 16 * 1024 * 1024);
    static ref DEFAULT_SLAB_BUCKETS: SlabAllocationBucketsPhys =
        get_tunable("default_slab_buckets", SlabAllocationBucketsPhys::default());
    static ref SLAB_CONDENSE_PER_CHECKPOINT: u64 = get_tunable("slab_condense_per_checkpoint", 10);

    // The target amount of free space that should be contained in free slabs, as a percentage;
    // i.e. 50% of all free space within the allocator, should be contained in free slabs.
    // The special value of "0" can be used to diable rebalancing entirely.
    static ref SLAB_REBALANCING_TARGET_FREE_SLABS_PCT: u64 =
        get_tunable("slab_rebalancing_target_free_slabs_pct", 50);
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlabId(u64);
impl SlabId {
    fn as_index(&self) -> usize {
        usize::from64(self.0)
    }

    pub fn next(&self) -> SlabId {
        SlabId(self.0 + 1)
    }
}

impl Add<u64> for SlabId {
    type Output = SlabId;
    fn add(self, rhs: u64) -> SlabId {
        SlabId(self.0 + rhs)
    }
}

impl Sub<SlabId> for SlabId {
    type Output = u64;
    fn sub(self, rhs: SlabId) -> u64 {
        self.0 - rhs.0
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlabGeneration(u64);
impl SlabGeneration {
    pub fn next(&self) -> SlabGeneration {
        SlabGeneration(self.0 + 1)
    }
}

trait SlabTrait {
    fn import_alloc(&mut self, extent: Extent);
    fn import_free(&mut self, extent: Extent);
    fn allocate(&mut self, size: u32) -> Option<Extent>;
    fn free(&mut self, extent: Extent);
    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap);
    fn condense_to_spacemap(&self, spacemap: &mut SpaceMap);
    fn max_size(&self) -> u32;
    fn capacity_bytes(&self) -> u64;
    fn free_space(&self) -> u64;
    fn allocated_space(&self) -> u64;
    fn phys_type(&self) -> SlabPhysType;
    fn num_segments(&self) -> u64;
    fn allocated_extents(&self) -> Vec<Extent>;
    fn dump_info(&self);
}

struct BitmapSlab {
    allocatable: RoaringBitmap,
    allocating: RoaringBitmap,
    freeing: RoaringBitmap,

    total_slots: u32,
    slot_size: u32,
    location: DiskLocation,
}

impl BitmapSlab {
    fn new_slab(id: SlabId, generation: SlabGeneration, extent: Extent, block_size: u32) -> Slab {
        let slab_size: u32 = extent.size.try_into().unwrap();
        let free_slots = slab_size / block_size;
        let mut allocatable = RoaringBitmap::new();

        allocatable.insert_range(0..free_slots.into());
        assert_eq!(allocatable.len(), u64::from(free_slots));

        Slab::new(
            id,
            generation,
            SlabType::BitmapBased(BitmapSlab {
                allocatable,
                allocating: Default::default(),
                freeing: Default::default(),
                total_slots: free_slots,
                slot_size: block_size,
                location: extent.location,
            }),
        )
    }

    fn slot_to_location(&self, slot: u32) -> DiskLocation {
        self.location + u64::from(slot * self.slot_size)
    }

    fn slab_end(&self) -> DiskLocation {
        self.slot_to_location(self.total_slots)
    }

    fn verify_contains(&self, extent: Extent) {
        assert_eq!(extent.location.disk, self.location.disk);
        assert_eq!(extent.size % u64::from(self.max_size()), 0);
        assert_ge!(extent.location, self.location);
        assert_le!(extent.location + extent.size, self.slab_end());
    }

    fn import_extent_impl(&mut self, extent: Extent, is_alloc: bool) {
        self.verify_contains(extent);

        let internal_offset = u32::try_from(extent.location.offset - self.location.offset).unwrap();
        assert_eq!(internal_offset % self.slot_size, 0);
        let num_slots = u32::try_from(extent.size).unwrap() / self.slot_size;
        assert_ge!(num_slots, 1);

        let first_slot = internal_offset / self.slot_size;
        assert_le!(first_slot + num_slots, self.total_slots);
        let slot_range = first_slot.into()..u64::from(first_slot + num_slots);
        if is_alloc {
            let removed = self.allocatable.remove_range(slot_range);
            assert_eq!(
                removed,
                u64::from(num_slots),
                "double alloc detected during import"
            );
        } else {
            let inserted = self.allocatable.insert_range(slot_range);
            assert_eq!(
                inserted,
                u64::from(num_slots),
                "double free detected during import"
            );
            assert_lt!(
                self.allocatable.max().unwrap(),
                self.total_slots,
                "FREE segment crosses the slab's end boundary"
            )
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
        let inserted = self.allocating.insert(slot);
        assert!(inserted);
        self.allocatable.remove(slot);

        // Cannot be allocating a block that's currently in the
        // middle of being freed.
        assert!(!self.freeing.contains(slot));

        Some(Extent {
            location: self.slot_to_location(slot),
            size: self.slot_size.into(),
        })
    }

    fn free(&mut self, extent: Extent) {
        self.verify_contains(extent);

        let internal_offset = u32::try_from(extent.location.offset - self.location.offset).unwrap();
        assert_eq!(internal_offset % self.slot_size, 0);

        let slot = internal_offset / self.slot_size;
        assert!(
            !self.allocatable.contains(slot),
            "double free at slot {:?}",
            slot
        );

        if self.allocating.contains(slot) {
            assert!(!self.freeing.contains(slot));
            let removed = self.allocating.remove(slot);
            assert!(removed);
        } else {
            let inserted = self.freeing.insert(slot);
            assert!(inserted);
        }
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        // It could happen that a segment was allocated and then freed within
        // the same checkpoint period at which point it would be part of both
        // `allocating` and `freeing` sets. For this reason we always record
        // `allocating` first, before `freeing`, on our spacemaps. Note that
        // segments cannot be freed and then allocated within the same
        // checkpoint period.
        for (first, last) in self.allocating.iter_ranges() {
            assert_ge!(last, first);
            spacemap.alloc(Extent {
                location: self.slot_to_location(first),
                size: u64::from((last - first + 1) * self.slot_size),
            });
        }
        self.allocating.clear();

        for (first, last) in self.freeing.iter_ranges() {
            assert_ge!(last, first);
            spacemap.free(Extent {
                location: self.slot_to_location(first),
                size: u64::from((last - first + 1) * self.slot_size),
            });
        }
        // Space freed during this checkpoint is now available for reallocation.
        for slot in self.freeing.iter() {
            let inserted = self.allocatable.insert(slot);
            assert!(inserted);
        }
        self.freeing.clear();
    }

    fn condense_to_spacemap(&self, spacemap: &mut SpaceMap) {
        // TODO: In the future we may want to check if writing the whole
        //       RoaringBitmap as a first-class spacemap entry is more
        //       practical here.
        let mut written_slots = 0;
        for (first, last) in self.allocatable.iter_inverse_ranges(self.total_slots) {
            assert_ge!(last, first);
            spacemap.alloc(Extent {
                location: self.slot_to_location(first),
                size: u64::from((last - first + 1) * self.slot_size),
            });
            written_slots += u64::from(last - first + 1);
        }
        assert_eq!(
            written_slots,
            u64::from(self.total_slots) - self.allocatable.len()
        );

        // In our attempt to make this independent of flush_to_spacemap(), we do
        // not mutate any of the in-memory data structures and mark all entries
        // from the allocating bitmap as free. The latter is because these
        // entries will be later marked as allocated in flush_to_spacemap().
        for (first, last) in self.allocating.iter_ranges() {
            spacemap.free(Extent {
                location: self.slot_to_location(first),
                size: u64::from((last - first + 1) * self.slot_size),
            });
        }
    }

    fn max_size(&self) -> u32 {
        self.slot_size
    }

    fn capacity_bytes(&self) -> u64 {
        // Compute from slot size rather than return slab size since the slot size
        // may not evenly divide the slab size, so some slab space may not be available.
        u64::from(self.total_slots * self.slot_size)
    }

    fn free_space(&self) -> u64 {
        self.allocatable.len() * u64::from(self.slot_size)
    }

    fn allocated_space(&self) -> u64 {
        (u64::from(self.total_slots) - self.allocatable.len()) * u64::from(self.slot_size)
    }

    fn phys_type(&self) -> SlabPhysType {
        SlabPhysType::BitmapBased {
            block_size: self.slot_size,
        }
    }

    fn dump_info(&self) {
        let used_slots = self.total_slots - u32::try_from(self.allocatable.len()).unwrap();
        println!(
            "slab_offset: {} slot_size: {} slots_used: {}/{} utilization: {}%",
            self.location.offset,
            nice_p2size(u64::from(self.slot_size)),
            used_slots,
            self.total_slots,
            (used_slots * 100) / self.total_slots
        );
        for (first, last) in self.allocatable.iter_inverse_ranges(self.total_slots) {
            let first_offset = self.slot_to_location(first);
            let last_offset = self.slot_to_location(last + 1);
            println!(
                "\tALLOC {:?} offset: [{}, {}) length: {} - slots: [{}, {}) count: {}",
                first_offset.disk,
                first_offset.offset,
                last_offset.offset,
                nice_p2size(last_offset - first_offset),
                first,
                last + 1,
                last - first + 1
            );
        }
        println!();
    }

    fn num_segments(&self) -> u64 {
        self.allocatable
            .iter_inverse_ranges(self.total_slots)
            .count() as u64
    }

    // Each extent may cover multiple adjacent allocated slots/blocks on disk. Additionally,
    // the list of extents are sorted in no particular order.
    fn allocated_extents(&self) -> Vec<Extent> {
        let mut all = RoaringBitmap::new();
        all.insert_range(0..u64::from(self.total_slots));
        let allocated = all - &self.allocatable - &self.freeing;

        allocated
            .iter_ranges()
            .map(|(first, last)| {
                let size = (last - first + 1) * self.slot_size;
                let location = self.slot_to_location(first);
                Extent {
                    size: size.into(),
                    location,
                }
            })
            .collect()
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
    fn new_slab(
        id: SlabId,
        generation: SlabGeneration,
        extent: Extent,
        max_allowed_alloc_size: u32,
    ) -> Slab {
        let mut allocatable: RangeTree = Default::default();
        allocatable.add(extent.location.offset, extent.size);
        Slab::new(
            id,
            generation,
            SlabType::ExtentBased(ExtentSlab {
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
                self.allocatable.remove(allocatable_offset, size);
                self.allocating.add(allocatable_offset, size);
                self.last_location = allocatable_offset + size;
                return Some(Extent::new(self.location.disk, allocatable_offset, size));
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
        self.allocatable.remove(extent.location.offset, extent.size);
    }

    fn import_free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);
        self.allocatable.add(extent.location.offset, extent.size);
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        assert_le!(size, self.max_size());
        let request_size = u64::from(size);
        // find next segment where this fits
        match self.allocate_impl(request_size, self.last_location, u64::MAX) {
            Some(e) => Some(e),
            None => self.allocate_impl(request_size, 0, self.last_location),
        }
    }

    fn free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);

        let offset = extent.location.offset;
        let size = extent.size;

        self.allocatable.verify_absent(offset, size);

        match self.allocating.overlap(offset, size) {
            Some(_) => {
                self.freeing.verify_absent(offset, size);
                self.allocating.remove(offset, size);
            }
            None => {
                self.allocating.verify_absent(offset, size);
                self.freeing.add(offset, size);
            }
        }
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        self.freeing.verify_space();
        self.allocating.verify_space();
        self.allocatable.verify_space();

        let disk = self.location.disk;

        // It could happen that a segment was allocated and then freed within
        // the same checkpoint period at which point it would be part of both
        // `allocating` and `freeing` sets. For this reason we always record
        // `allocating` first, before `freeing`, on our spacemaps. Note that
        // segments cannot be freed and then allocated within the same
        // checkpoint period.
        for (&start, &size) in self.allocating.iter() {
            self.allocatable.verify_absent(start, size);
            spacemap.alloc(Extent::new(disk, start, size));
        }
        self.allocating.clear();

        // Space freed during this checkpoint is now available for reallocation.
        for (&start, &size) in self.freeing.iter() {
            self.allocating.verify_absent(start, size);
            spacemap.free(Extent::new(disk, start, size));
            self.allocatable.add(start, size);
        }
        self.freeing.clear();
    }

    fn condense_to_spacemap(&self, spacemap: &mut SpaceMap) {
        let disk = self.location.disk;

        for (offset, size) in self
            .allocatable
            .iter_inverse(self.location.offset, self.slab_end().offset)
        {
            spacemap.alloc(Extent::new(disk, offset, size));
        }

        // In our attempt to make this independent of flush_to_spacemap(), we do
        // not mutate any of the in-memory data structures and mark all entries
        // from the allocating tree as free. The latter is because these entries
        // will be later marked as allocated in flush_to_spacemap().
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

    fn allocated_space(&self) -> u64 {
        self.total_space - self.free_space()
    }

    fn phys_type(&self) -> SlabPhysType {
        SlabPhysType::ExtentBased {
            max_size: self.max_allowed_alloc_size,
        }
    }

    fn dump_info(&self) {
        println!(
            "slab_offset: {} max_allowed_alloc_size: {} allocated_bytes: {} utilization: {}%",
            self.location.offset,
            nice_p2size(u64::from(self.max_allowed_alloc_size)),
            nice_p2size(self.total_space - self.allocatable.space()),
            ((self.total_space - self.allocatable.space()) * 100) / self.total_space
        );
        for (offset, size) in self
            .allocatable
            .iter_inverse(self.location.offset, self.slab_end().offset)
        {
            println!(
                "\tALLOC offset: [{}  {}) length: {}",
                offset,
                offset + size,
                nice_p2size(size),
            );
        }
        println!();
    }

    fn num_segments(&self) -> u64 {
        self.allocatable
            .iter_inverse(self.location.offset, self.slab_end().offset)
            .count() as u64
    }

    fn allocated_extents(&self) -> Vec<Extent> {
        let mut allocated: RangeTree = Default::default();

        self.allocatable
            .iter_inverse(self.location.offset, self.slab_end().offset)
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
            .map(|(&offset, &size)| Extent::new(self.location.disk, offset, size))
            .collect()
    }
}

struct FreeSlab {
    extent: Extent,
}

impl FreeSlab {
    fn new_slab(id: SlabId, generation: SlabGeneration, extent: Extent) -> Slab {
        Slab::new(id, generation, SlabType::Free(FreeSlab { extent }))
    }
}

impl SlabTrait for FreeSlab {
    fn import_alloc(&mut self, extent: Extent) {
        panic!("attempting to import alloc {:?} on free slab", extent);
    }

    fn import_free(&mut self, extent: Extent) {
        panic!("attempting to import free {:?} on free slab", extent);
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        panic!(
            "attempting to allocate block from free slab: size = {}",
            size
        );
    }

    fn free(&mut self, extent: Extent) {
        panic!("attempting to free block from free slab: {:?}", extent);
    }

    fn flush_to_spacemap(&mut self, _: &mut SpaceMap) {
        panic!("attempting to flush free slab",);
    }

    fn condense_to_spacemap(&self, _: &mut SpaceMap) {
        // Nothing to condense for free slabs
    }

    fn max_size(&self) -> u32 {
        panic!("free slab doesn't have a maximum allocation size");
    }

    fn capacity_bytes(&self) -> u64 {
        self.extent.size
    }

    fn free_space(&self) -> u64 {
        self.extent.size
    }

    fn allocated_space(&self) -> u64 {
        0
    }

    fn phys_type(&self) -> SlabPhysType {
        SlabPhysType::Free
    }

    fn dump_info(&self) {
        println!("{:?}", self.extent);
        println!();
    }

    fn num_segments(&self) -> u64 {
        0
    }

    fn allocated_extents(&self) -> Vec<Extent> {
        vec![]
    }
}

struct EvacuatingSlab {
    extent: Extent,
}

impl EvacuatingSlab {
    fn new_slab(id: SlabId, generation: SlabGeneration, extent: Extent) -> Slab {
        Slab::new(
            id,
            generation,
            SlabType::Evacuating(EvacuatingSlab { extent }),
        )
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

    fn flush_to_spacemap(&mut self, _: &mut SpaceMap) {
        panic!("attempting to flush evacuating slab",);
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

    fn allocated_space(&self) -> u64 {
        0
    }

    fn phys_type(&self) -> SlabPhysType {
        SlabPhysType::Evacuating
    }

    fn dump_info(&self) {
        println!("{:?}", self.extent);
        println!();
    }

    fn num_segments(&self) -> u64 {
        0
    }

    fn allocated_extents(&self) -> Vec<Extent> {
        panic!("evacuating slab doesn't track allocated extents");
    }
}

enum SlabType {
    BitmapBased(BitmapSlab),
    ExtentBased(ExtentSlab),
    Evacuating(EvacuatingSlab),
    Free(FreeSlab),
}

impl SlabType {
    fn with_trait_mut<R, F>(&mut self, cb: F) -> R
    where
        F: FnOnce(&mut dyn SlabTrait) -> R,
    {
        match self {
            SlabType::BitmapBased(t) => cb(t),
            SlabType::ExtentBased(t) => cb(t),
            SlabType::Evacuating(t) => cb(t),
            SlabType::Free(t) => cb(t),
        }
    }

    fn with_trait<R, F>(&self, cb: F) -> R
    where
        F: FnOnce(&dyn SlabTrait) -> R,
    {
        match self {
            SlabType::BitmapBased(t) => cb(t),
            SlabType::ExtentBased(t) => cb(t),
            SlabType::Evacuating(t) => cb(t),
            SlabType::Free(t) => cb(t),
        }
    }
}

struct Slab {
    id: SlabId,
    generation: SlabGeneration,
    info: SlabType,
    is_dirty: bool,
}

impl Slab {
    fn new(id: SlabId, generation: SlabGeneration, info: SlabType) -> Slab {
        Slab {
            id,
            generation,
            info,
            is_dirty: false,
        }
    }

    fn import_alloc(&mut self, extent: Extent) {
        self.info.with_trait_mut(|t| t.import_alloc(extent));
    }

    fn import_free(&mut self, extent: Extent) {
        self.info.with_trait_mut(|t| t.import_free(extent));
    }

    fn allocate(&mut self, size: u32) -> Option<Extent> {
        self.info.with_trait_mut(|t| t.allocate(size))
    }

    fn free(&mut self, extent: Extent) {
        self.info.with_trait_mut(|t| t.free(extent));
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        self.info.with_trait_mut(|t| t.flush_to_spacemap(spacemap));
        self.is_dirty = false;
    }

    fn condense_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        // Bump the generation of this slab since we are condensing it and
        // writing it to the new spacemap. By bumping the generation we also
        // make the entries in the old spacemap obsolete.
        self.generation = self.generation.next();
        spacemap.mark_generation(self.id, self.generation);
        self.info
            .with_trait_mut(|t| t.condense_to_spacemap(spacemap));
    }

    fn max_size(&self) -> u32 {
        self.info.with_trait(|t| t.max_size())
    }

    fn free_space(&self) -> u64 {
        self.info.with_trait(|t| t.free_space())
    }

    fn allocated_space(&self) -> u64 {
        self.info.with_trait(|t| t.allocated_space())
    }

    fn capacity_bytes(&self) -> u64 {
        self.info.with_trait(|t| t.capacity_bytes())
    }

    fn num_segments(&self) -> u64 {
        self.info.with_trait(|t| t.num_segments())
    }

    fn allocated_extents(&self) -> Vec<Extent> {
        self.info.with_trait(|t| t.allocated_extents())
    }

    fn get_phys(&self) -> SlabPhys {
        SlabPhys {
            generation: self.generation,
            slab_type: self.info.with_trait(|t| t.phys_type()),
        }
    }

    fn to_sorted_slab_entry(&self) -> SortedSlabEntry {
        SortedSlabEntry {
            allocated_space: self.allocated_space(),
            slab_id: self.id,
        }
    }

    fn dump_info(&self) {
        println!("{:?} {:?}", self.id, self.generation);
        self.info.with_trait(|t| t.dump_info());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SortedSlabEntry {
    allocated_space: u64,
    slab_id: SlabId,
}

struct SortedSlabs {
    is_extent_based: bool,
    by_freeness: BTreeSet<SortedSlabEntry>,
    last_allocated: Option<SortedSlabEntry>,
    been_through_once: bool,
}

impl SortedSlabs {
    fn new<I>(is_extent_based: bool, iter: I) -> SortedSlabs
    where
        I: IntoIterator<Item = SortedSlabEntry>,
    {
        let mut by_freeness = BTreeSet::default();
        for x in iter {
            by_freeness.insert(x);
        }
        let last_allocated = by_freeness.iter().next().copied();
        SortedSlabs {
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
            // If this clause is hit it means that we've been through all
            // the slabs in this SortedSlab set and we've also filled up
            // a slab that we just created and inserted to the set. In
            // order to not iterate through all the slabs again for this
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
            // We've iterated through all the existing slabs in
            // the SortedSlab set for this checkpoint. This flag
            // will be reset when we re-create this SortedSlabs
            // at the end of the checkpoint.
            self.been_through_once = true;
        }
        self.get_current()
    }

    fn insert(&mut self, entry: SortedSlabEntry) {
        self.by_freeness.insert(entry);
        self.last_allocated = Some(entry);
    }

    fn remove(&mut self, id: SlabId) {
        if let Some(last) = self.last_allocated {
            // If we are removing the slab that matches the allocation cursor, we need to ensure we advance the cursor
            // so that future allocations don't attempt to allocate from this removed slab.
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
// Note: Even though not strictly necessary, in general
// the BitmapBased slabs are before all the ExtentBased
// ones (i.e. Bitmaps are used for smaller allocation sizes).
struct SlabAllocationBuckets(BTreeMap<u32, SortedSlabs>);

impl SlabAllocationBuckets {
    fn new(
        phys: SlabAllocationBucketsPhys,
        mut slabs: BTreeMap<u32, Vec<SortedSlabEntry>>,
    ) -> Self {
        let mut buckets = BTreeMap::new();
        for (max_size, is_extent_based) in phys.buckets {
            buckets.insert(
                max_size,
                SortedSlabs::new(is_extent_based, slabs.remove(&max_size).unwrap_or_default()),
            );
        }

        // We expect for all slabs passed in to be consumed and added to a bucket.
        assert!(slabs.is_empty());

        SlabAllocationBuckets(buckets)
    }

    fn get_bucket_for_allocation_size(&mut self, request_size: u32) -> (&u32, &mut SortedSlabs) {
        self.0
            .range_mut(request_size..)
            .next()
            .expect("allocation request larger than largest configured slab type")
    }

    fn remove_slab(&mut self, slab: &Slab) {
        let bucket = slab.max_size();
        self.0.get_mut(&bucket).unwrap().remove(slab.id);
    }
}

struct Slabs(Vec<Slab>);

impl Slabs {
    fn get(&self, id: SlabId) -> &Slab {
        &self.0[id.as_index()]
    }

    fn get_mut(&mut self, id: SlabId) -> &mut Slab {
        &mut self.0[id.as_index()]
    }

    async fn open(
        capacity: &BiBTreeMap<Extent, SlabId>,
        spacemap: &SpaceMap,
        spacemap_next: &SpaceMap,
        slab_size: u32,
        slabs_phys: &TerseVec<SlabPhys>,
    ) -> Self {
        let mut extent_iter = capacity.iter().map(|(&extent, _)| extent);
        let mut current_extent = extent_iter.next().unwrap();

        let mut slabs = Slabs(
            slabs_phys
                .0
                .iter()
                .enumerate()
                .map(|(slab_id, phys_slab)| {
                    let sid = SlabId(slab_id as u64);

                    if current_extent.size < slab_size.into() {
                        current_extent = extent_iter.next().unwrap();
                    }

                    let slab_extent = current_extent.range(0, slab_size.into());
                    current_extent = current_extent
                        .range(slab_size.into(), current_extent.size - u64::from(slab_size));

                    match phys_slab.slab_type {
                        SlabPhysType::BitmapBased { block_size } => {
                            BitmapSlab::new_slab(sid, phys_slab.generation, slab_extent, block_size)
                        }
                        SlabPhysType::ExtentBased { max_size } => {
                            ExtentSlab::new_slab(sid, phys_slab.generation, slab_extent, max_size)
                        }
                        SlabPhysType::Free => {
                            FreeSlab::new_slab(sid, phys_slab.generation, slab_extent)
                        }
                        SlabPhysType::Evacuating => {
                            EvacuatingSlab::new_slab(sid, phys_slab.generation, slab_extent)
                        }
                    }
                })
                .collect(),
        );

        // There should be no leftover capacity; it should have all been consumed by the slabs_phys.
        assert_lt!(current_extent.size, slab_size.into());
        assert!(extent_iter.next().is_none());

        let mut slab_import_generations = vec![SlabGeneration(0); slabs.0.len()];
        let mut import_cb = |entry| match entry {
            SpaceMapEntry::Alloc(extent) => {
                let slab_id =
                    BlockAllocator::slab_id_from_extent_impl(capacity, slab_size.into(), extent);
                if slabs.get(slab_id).generation == slab_import_generations[slab_id.as_index()] {
                    slabs.get_mut(slab_id).import_alloc(extent)
                }
            }
            SpaceMapEntry::Free(extent) => {
                let slab_id =
                    BlockAllocator::slab_id_from_extent_impl(capacity, slab_size.into(), extent);
                if slabs.get(slab_id).generation == slab_import_generations[slab_id.as_index()] {
                    slabs.get_mut(slab_id).import_free(extent)
                }
            }
            SpaceMapEntry::MarkGeneration(mark) => {
                assert_ge!(
                    mark.generation,
                    slab_import_generations[mark.slab_id.as_index()]
                );
                slab_import_generations[mark.slab_id.as_index()] = mark.generation;
            }
        };
        spacemap.load(&mut import_cb).await;
        spacemap_next.load(&mut import_cb).await;

        slabs
    }
}

pub struct BlockAllocator {
    capacity: BiBTreeMap<Extent, SlabId>,
    slab_size: u32,

    // # Spacemap Condensing - Design Overview
    //
    // We need to condense our spacemap in order to not run out of space.
    //
    // In a scheme where one spacemap is used to log the changes from all
    // the slabs, condensing would be expensive for workloads where there
    // are lots of incoming allocations/frees because these changes would
    // need to wait for condensing to be done before they are applied.
    //
    // On the other hand, having one spacemap per slab and choosing how
    // many of them to condense dynamically based on the workload could
    // be a viable option. Unfortunately, it comes with its own set of
    // problems too. Specifically, for big devices that have a lot of
    // slabs with a small amount of pending changes each, condensing would
    // cause scattered I/0s whose block size won't be fully utilized,
    // affecting our overall bandwidth as a result.
    //
    // The above antithetical designs highlight a tension in the number
    // of spacemaps we choose to represent our slabs and the problems that
    // come up if you have too many or too little of them. Picking the
    // right number of spacemaps is hard, primarily because that number
    // is workload dependend and dynamically changing it is not something
    // that can be done in a straightforward manner.
    //
    // For this block allocator we decided to approach things differently.
    // We use a two spacemap scheme (`spacemap` and `spacemap_next`) where
    // a certain number of slabs are condensed in a round-robin fashion every
    // checkpoint. Initially all slabs flush their changes to the first
    // spacemap (`spacemap`). Whenever a slab is condensed, we place its
    // condensed entries/representation to the second spacemap (`spacemap_next`).
    // Every subsequent changes/flushes for that slab are also placed on that
    // spacemap. Once we've done a full circle and all slabs have been moved
    // to `spacemap_next`, then `spacemap` is no longer needed. At that point
    // we get rid of `spacemap`, replacing it with `spacemap_next`, and
    // use an empty spacemap as `spacemap_next` for our next round of condensing.
    //
    // With the above design we use at most 2 I/Os where we expect the blocksize
    // to be utilized as the two spacemaps represent all the slabs in the
    // Zettacache. Furthermore, we can dynamically adjust the condensing rate
    // (TODO: see DOSE-629) however we see fit, making sure that our spacemaps
    // don't grow too long and that condensing itself doesn't interfere too much
    // with other activity.
    spacemap: SpaceMap,
    spacemap_next: SpaceMap,
    next_slab_to_condense: SlabId,

    slabs: Slabs,
    dirty_slabs: Vec<SlabId>,
    free_slabs: Vec<SlabId>,
    evacuating_slabs: Vec<SlabId>,

    slab_buckets: SlabAllocationBuckets,

    available_space: u64,
    freeing_space: u64,

    block_access: Arc<BlockAccess>,
}

impl BlockAllocator {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: BlockAllocatorPhys,
    ) -> BlockAllocator {
        let begin = Instant::now();
        let spacemap = SpaceMap::open(
            block_access.clone(),
            extent_allocator.clone(),
            phys.spacemap,
        );
        let spacemap_next = SpaceMap::open(
            block_access.clone(),
            extent_allocator.clone(),
            phys.spacemap_next,
        );
        let slab_size = phys.slab_size;
        let capacity: BiBTreeMap<Extent, SlabId> = {
            let mut id = SlabId(0);
            phys.capacity
                .into_iter()
                .map(|extent| {
                    let start = id;
                    id = id + extent.size / u64::from(slab_size);
                    (extent, start)
                })
                .collect()
        };
        let slabs = Slabs::open(&capacity, &spacemap, &spacemap_next, slab_size, &phys.slabs).await;

        let mut available_space = 0u64;
        let mut free_slabs = Vec::new();
        let mut evacuating_slabs = Vec::new();
        let mut slabs_by_bucket: BTreeMap<u32, Vec<SortedSlabEntry>> = BTreeMap::new();
        for slab in slabs.0.iter() {
            available_space += slab.free_space();

            match &slab.info {
                SlabType::BitmapBased(_) | SlabType::ExtentBased(_) => {
                    slabs_by_bucket
                        .entry(slab.max_size())
                        .or_default()
                        .push(slab.to_sorted_slab_entry());
                }
                SlabType::Free(_) => {
                    free_slabs.push(slab.id);
                }
                SlabType::Evacuating(_) => {
                    evacuating_slabs.push(slab.id);
                }
            }
        }
        // So that we'll hit multiple disks.
        free_slabs.shuffle(&mut thread_rng());

        let slab_buckets = SlabAllocationBuckets::new(phys.slab_buckets, slabs_by_bucket);

        info!(
            "loaded BlockAllocator metadata in {}ms",
            begin.elapsed().as_millis()
        );

        BlockAllocator {
            capacity,
            slab_size,
            spacemap,
            spacemap_next,
            next_slab_to_condense: phys.next_slab_to_condense,
            slabs,
            dirty_slabs: Default::default(),
            free_slabs,
            evacuating_slabs,
            slab_buckets,
            available_space,
            freeing_space: 0,
            block_access,
        }
    }

    fn dirty_slab_id(&mut self, slab_id: SlabId) {
        let slab = self.slabs.get_mut(slab_id);
        if !slab.is_dirty {
            self.dirty_slabs.push(slab_id);
            slab.is_dirty = true;
        }
    }

    fn allocate_from_new_slab(&mut self, request_size: u32) -> Option<Extent> {
        let new_id = match self.free_slabs.pop() {
            Some(id) => id,
            None => {
                return None;
            }
        };
        let extent = self.slab_extent_from_id(new_id);
        let slab_next_generation = self.slabs.get(new_id).generation.next();

        let (&max_allocation_size, sorted_slabs) = self
            .slab_buckets
            .get_bucket_for_allocation_size(request_size);

        let mut new_slab = if sorted_slabs.is_extent_based {
            ExtentSlab::new_slab(new_id, slab_next_generation, extent, max_allocation_size)
        } else {
            BitmapSlab::new_slab(new_id, slab_next_generation, extent, max_allocation_size)
        };
        let target_spacemap = if self.next_slab_to_condense <= new_id {
            &mut self.spacemap
        } else {
            &mut self.spacemap_next
        };
        target_spacemap.mark_generation(new_id, slab_next_generation);
        sorted_slabs.insert(new_slab.to_sorted_slab_entry());

        let extent = new_slab.allocate(request_size);
        assert!(extent.is_some());
        assert!(matches!(self.slabs.get(new_id).info, SlabType::Free(_)));
        *self.slabs.get_mut(new_id) = new_slab;
        self.dirty_slab_id(new_id);
        trace!("{:?} added to {} byte bucket", new_id, max_allocation_size,);
        self.available_space -= extent.unwrap().size;
        extent
    }

    pub fn allocate(&mut self, request_size: u32) -> Option<Extent> {
        assert_ge!(self.slab_size, request_size);

        // Note: we assume allocation sizes are guaranteed to be aligned
        // from the caller for now.
        self.block_access.verify_aligned(request_size);

        let (&max_allocation_size, sorted_slabs) = self
            .slab_buckets
            .get_bucket_for_allocation_size(request_size);
        let slabs_in_bucket = sorted_slabs.by_freeness.len();

        // TODO - WIP Allocation Algorithm
        //
        // The current naive implemenation of the allocation is the following:
        // - We are iterating over the slabs of the our allocation bucket in
        //   sorted order from the slabs with the most free space to the ones
        //   with the least free space (according to their free space accounting
        //   since our latest flush/checkpoint).
        // - We are looking at the current slab used since our last allocation
        //   (or the first slab if this is the first allocation since the last
        //    checkpoint), and try to allocate from that.
        // - If the allocation fails we move to the next slab in our set of
        //   sorted slabs, and try to allocate from that one.
        // - If that fails too, we keep trying through all the slabs in that
        //   set until we go through them all at which point we will try to
        //   convert a FreeSlab to this type, add it to the set, and allocate
        //   from it.
        // - If that fails too then we fail the allocation (and any allocation
        //   for that allocation size until the next flush/checkpoint).
        //
        // Obviously this is far from ideal but it is deterministic and easy
        // to reason about for now.
        //
        loop {
            match sorted_slabs.get_current() {
                Some(id) => match self.slabs.get_mut(id).allocate(request_size) {
                    Some(extent) => {
                        trace!(
                            "satisfied {} byte allocation request: {:?}",
                            request_size,
                            extent
                        );
                        self.dirty_slab_id(id);
                        self.available_space -= extent.size;
                        return Some(extent);
                    }
                    None => {
                        let debug = sorted_slabs.advance();
                        trace!(
                            "advance slab bucket {} cursor to {:?}",
                            max_allocation_size,
                            debug
                        );
                    }
                },
                None => match self.allocate_from_new_slab(request_size) {
                    Some(extent) => {
                        trace!(
                            "satisfied {} byte allocation request: {:?}",
                            request_size,
                            extent
                        );
                        return Some(extent);
                    }
                    None => {
                        trace!(
                            "allocation of {} bytes failed; no free slabs left; {} slabs used for {} byte bucket",
                            request_size,
                            slabs_in_bucket,
                            max_allocation_size
                        );
                        return None;
                    }
                },
            }
        }
    }

    pub fn free(&mut self, extent: Extent) {
        self.block_access.verify_aligned(extent.location.offset);
        self.block_access.verify_aligned(extent.size);
        trace!("free request: {:?}", extent);

        let slab_id = self.slab_id_from_extent(extent);
        self.slabs.get_mut(slab_id).free(extent);
        self.freeing_space += extent.size;
        self.dirty_slab_id(slab_id);
    }

    pub fn rebalance_needed(&self) -> bool {
        self.num_slabs_to_rebalance() != 0
    }

    //
    // This function is the entry-point to starting the cache rebalancing process. This will select which slab(s)
    // need to be rebalanced, mark those slabs as undergoing evacuation, and allocate new disk locations for the
    // data currently stored on those slabs. The value returned by this function is a map of key-value pairs,
    // where the key denotes the current location on disk (i.e. on the slab(s) being evacuated), and the value is
    // the newly allocated disk location where the data should be moved (to facilitate the evacuation).
    //
    // It is the responsibility of the consumer of this function to actually move the data from the old location,
    // to the new location. This way, the allocator remains responsible only for the allocation of disk extents,
    // and not for the reading and writing of the block data.
    //
    // Once the consumer has finished the process of moving the data blocks to their new locations, it is also the
    // consumer's responsibility to call the "rebalance_fini" function, marking the rebalance process as finished.
    // This allows the allocator to transition the slabs that were undergoing evacuation to free slabs, such that
    // the slabs can later be used for allocation.
    //
    pub fn rebalance_init(&mut self) -> Option<BTreeMap<Extent, DiskLocation>> {
        if *SLAB_REBALANCING_TARGET_FREE_SLABS_PCT == 0 {
            trace!("rebalance requested, but not enabled");
            return None;
        }

        // For now, ensure rebalance_fini() is called before this function can be called a second time.
        assert!(self.evacuating_slabs.is_empty());

        let slabs = self.slabs_to_rebalance();
        if slabs.is_empty() {
            info!("cache rebalance is not needed");
            return None;
        }

        info!("initializing rebalance of {} slabs", slabs.len());

        // In order to ensure the allocations performed in rebalance_slab() (called below) are not satisfied by any
        // of the slabs we're going to rebalance, we need to remove these slabs from the list of slabs available
        // for allocation. Further, we must remove all slabs before we do any allocations, to ensure we don't move
        // an extent multiple times; otherwise, data corruption could occur, as the data contained in the extents,
        // can be moved by the caller in any order.
        //
        // For example, if we mark an extent as moving from disk location A to B, and then again from B to C, the
        // final data contained at disk location C could be incorrect, if the caller does the move of B to C before
        // the move of A to B. Since we do not enforce the order in which the caller will do the copies, we need to
        // ensure this cannot happen, by never moving an extent more than once.
        for &id in slabs.iter() {
            trace!("prepping slab '{:?}' for rebalancing", id);
            self.slab_buckets.remove_slab(self.slabs.get(id));
        }

        let map: BTreeMap<Extent, DiskLocation> = slabs
            .iter()
            .map(|&id| self.rebalance_slab(id))
            .flatten()
            .collect();

        assert!(!self.evacuating_slabs.is_empty());
        Some(map)
    }

    fn num_slabs_to_rebalance(&self) -> u64 {
        let available = self.available();
        let target_number_of_free_slabs =
            (available * *SLAB_REBALANCING_TARGET_FREE_SLABS_PCT) / 100 / u64::from(self.slab_size);
        let current_number_of_free_slabs = self.free_slabs.len() as u64;

        target_number_of_free_slabs.saturating_sub(current_number_of_free_slabs)
    }

    fn slabs_to_rebalance(&self) -> Vec<SlabId> {
        let num_slabs_to_rebalance = self.num_slabs_to_rebalance();

        trace!(
            "attempting to find {} slabs to rebalance",
            num_slabs_to_rebalance
        );

        let mut free_space_per_bucket: BTreeMap<u32, u64> = self
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
        let slabs: BTreeSet<SortedSlabEntry> = self
            .slabs
            .0
            .iter()
            // TODO: Add support for rebalacing of non-bitmap based slab types.
            // The rebalance_slab() function does not currently handle allocation failures. Thus, we currently
            // only support the rebalancing of bitmap based slabs, as we can avoid allocation failures of these
            // slab types relatively easily.
            .filter(|&slab| matches!(slab.info, SlabType::BitmapBased(_)))
            .map(|slab| slab.to_sorted_slab_entry())
            .collect();

        // The goal of the rebalance, is to generate more free slabs, such that we can satisfy future allocations. It doesn't
        // matter which bucket the free slab came from; if the free slab comes from a very fragmented bucket, or a very compact
        // bucket, it doesn't really matter. The only thing that matters, is that we have free slabs available, such that future
        // allocations do not fail.
        //
        // Further, a secondary goal, is to accomplish the aformentioned primary goal, but while minimizing the cost of doing so;
        // i.e. minimizing the bytes read and written by the rebalacing process.
        //
        // As such, we select the slabs that we intend to rebalance, by seeking to rebalance the most free slabs first. This way,
        // we will choose the slabs that can be evacuated with the least about of data transfer (i.e. disk reads and writes),
        // regardless of the bucket the slab belongs too.
        slabs
            .iter()
            .filter_map(|entry| {
                let slab = self.slabs.get(entry.slab_id);

                // We currently only support rebalancing of bitmap based slabs; see comment above.
                // The logic below only works for bitmap based slabs.
                assert!(matches!(slab.info, SlabType::BitmapBased(_)));

                let bucket = slab.max_size();
                let bytes_free_in_bucket = free_space_per_bucket.get_mut(&bucket).unwrap();

                // If there's not enough free space in the bucket to completely evacuate this slab's allocated bytes,
                // then we skip it, and move on to the next slab in the (sorted) list. This way, we don't have to handle
                // allocation failures when rebalance_slab() is called.
                *bytes_free_in_bucket = bytes_free_in_bucket.checked_sub(slab.capacity_bytes())?;

                Some(slab.id)
            })
            .take(usize::try_from(num_slabs_to_rebalance).unwrap())
            .collect()
    }

    fn rebalance_slab(&mut self, id: SlabId) -> Vec<(Extent, DiskLocation)> {
        trace!("starting rebalance of slab '{:?}'", id);

        let slab = self.slabs.get(id);

        // We currently only support rebalancing of bitmap based slabs; see comment in rebalance_init().
        // The logic below only works for bitmap based slabs.
        assert!(matches!(slab.info, SlabType::BitmapBased(_)));

        let slot_size = u64::from(slab.max_size());

        // We must do the new allocations prior to transitioning the slab to an evacuating slab type, since
        // we don't support allocations from evacuating slabs.
        let map = slab
            .allocated_extents()
            .iter()
            // Note, this logic only works for bitmap based slabs, but that's fine, as we only support rebalancing of
            // bitmap based slabs.
            //
            // We need to perform a seperate allocation for each allocated slot in the bitmap extent we're attempting to
            // rebalance. This is so that the allocations we make, are guaranteed to be fulfilled by the same slab bucket
            // the data is currently contained in. We don't want to coalesce multiple slots into a single allocation, as
            // that would cause the new allocation to occur in a different sized slab bucket. Thus, this step converts
            // each allocated extent, representing a contiguous range of allocated slots in the bitmap slab, into multiple
            // extents, one for each allocated slot in the range. This way, the extent for each allocated slot can then be
            // used to perform the new allocations, and generate a mapping of old location to new location, for each allocated
            // slot in the bitmap slab.
            .flat_map(|&extent| {
                // For bitmap based slabs, allocations can only come in multiples of the slot size.
                assert_eq!(extent.size % slot_size, 0);

                (0..(extent.size / slot_size)).map(move |slot| Extent {
                    size: slot_size,
                    location: extent.location + (slot * slot_size),
                })
            })
            // Now that we have a unique extent for each allocated slot, we can perform the new allocation, marking the location
            // the old data should be moved to.
            .map(|old| {
                let size = u32::try_from(old.size).unwrap();

                // It's the caller's responsibility to ensure allocation failures do not occur.
                let new = self.allocate(size).unwrap_or_else(|| {
                    panic!("cache rebalance allocation failed for size: '{:?}'", size)
                });

                (old, new.location)
            })
            .collect();

        // Since evacuating slabs don't have any allocatable space, we must account for that here; we must do
        // this before we transition to an evacuating slab (evacuating slabs have no free space).
        let slab = self.slabs.get(id);
        self.available_space -= slab.free_space();

        trace!("marking slab '{:?}' as evacuating", id);

        self.evacuating_slabs.push(id);
        *self.slabs.get_mut(id) =
            EvacuatingSlab::new_slab(id, slab.generation.next(), self.slab_extent_from_id(id));

        map
    }

    // See comment above rebalance_init() for more details.
    pub fn rebalance_fini(&mut self) {
        for id in mem::take(&mut self.evacuating_slabs) {
            // evacuating slabs cannot allocate() or free(); thus, they should never be dirty.
            assert!(!self.slabs.get(id).is_dirty);

            trace!("marking slab '{:?}' as free", id);

            self.free_slabs.push(id);
            *self.slabs.get_mut(id) = FreeSlab::new_slab(
                id,
                self.slabs.get(id).generation.next(),
                self.slab_extent_from_id(id),
            );

            // Since we reduce the available space when transitioning a slab to be evacuating, we need to
            // ensure we increase the available space when transitioning the slab to be free. We must do
            // this after the slab has been marked a free slab, since evacuating slabs have no free space.
            self.available_space += self.slabs.get(id).free_space();
        }
    }

    pub async fn flush(&mut self) -> BlockAllocatorPhys {
        // We first condense any slabs so later when we flush any of them that
        // are dirty we've already migrated their entries of this checkpoint to
        // spacemap_next.
        let begin = Instant::now();
        let slabs_to_condense = min(
            *SLAB_CONDENSE_PER_CHECKPOINT,
            (self.slabs.0.len() - self.next_slab_to_condense.as_index()) as u64,
        );
        trace!(
            "condensing the next {} slabs starting from {:?}",
            slabs_to_condense,
            self.next_slab_to_condense
        );
        for _ in 0..slabs_to_condense {
            self.slabs
                .get_mut(self.next_slab_to_condense)
                .condense_to_spacemap(&mut self.spacemap_next);
            self.next_slab_to_condense = self.next_slab_to_condense.next();
        }
        if self.next_slab_to_condense.as_index() == self.slabs.0.len() {
            self.next_slab_to_condense = SlabId(0);
            self.spacemap.clear();
            mem::swap(&mut self.spacemap_next, &mut self.spacemap);
        }
        assert_lt!(self.next_slab_to_condense.as_index(), self.slabs.0.len());
        trace!(
            "spacemap has {} alloc and {} total entries",
            self.spacemap.alloc_entries(),
            self.spacemap.total_entries()
        );
        trace!(
            "spacemap_next has {} alloc and {} total entries",
            self.spacemap_next.alloc_entries(),
            self.spacemap_next.total_entries()
        );

        // Flush any dirty slabs. If any slab is completely empty mark it as free.
        // Keep track of the buckets/SortedSlabs sets that these dirty slabs belong
        // to so later we can update their slab order by freeness.
        trace!("flushing {} dirty slabs", self.dirty_slabs.len());
        let mut dirty_buckets = HashSet::new();
        for slab_id in std::mem::take(&mut self.dirty_slabs) {
            let extent = self.slab_extent_from_id(slab_id);
            let slab = self.slabs.get_mut(slab_id);

            // It's possible for a slab in the dirty list, to be converted to a different slab type, such that the actual
            // slab object is no longer dirty, but the slab's id is still in the dirty list. For example, if an already dirtied
            // slab is chosen to be rebalanced. Thus, prior to flushing the slab, we double check that the slab is still dirty.
            if !slab.is_dirty {
                continue;
            }

            let target_spacemap = if self.next_slab_to_condense <= slab_id {
                &mut self.spacemap
            } else {
                &mut self.spacemap_next
            };
            slab.flush_to_spacemap(target_spacemap);
            dirty_buckets.insert(slab.max_size());
            if slab.free_space() == u64::from(self.slab_size) {
                self.free_slabs.push(slab.id);
                *slab = FreeSlab::new_slab(slab_id, slab.generation.next(), extent);
                target_spacemap.mark_generation(slab.id, slab.generation);
            }
        }

        // So that we'll hit multiple disks.
        self.free_slabs.shuffle(&mut thread_rng());

        trace!("allocation buckets to be resorted: {:?}", dirty_buckets);

        // Update any buckets which we've performed any allocations/frees during
        // this checkpoint by recreating their SortedSlabs (which in turn
        // updates their order by freeness and also removes any empty slabs).
        for bucket_size in dirty_buckets {
            let bucket = self.slab_buckets.0.get_mut(&bucket_size).unwrap();
            let slabs = &mut self.slabs;
            let iter = bucket.by_freeness.iter().filter_map(|entry| {
                let slab = slabs.get(entry.slab_id);
                if let SlabType::Free(_) = slab.info {
                    None
                } else {
                    Some(slab.to_sorted_slab_entry())
                }
            });
            *bucket = SortedSlabs::new(bucket.is_extent_based, iter);
        }
        self.available_space += self.freeing_space;
        self.freeing_space = 0;

        let (spacemap, spacemap_next) =
            futures::future::join(self.spacemap.flush(), self.spacemap_next.flush()).await;

        let phys = BlockAllocatorPhys {
            capacity: self.capacity.iter().map(|(&extent, _)| extent).collect(),
            slab_size: self.slab_size,
            spacemap,
            spacemap_next,
            next_slab_to_condense: self.next_slab_to_condense,
            slabs: self
                .slabs
                .0
                .iter()
                .map(|slab| slab.get_phys())
                .collect::<Vec<_>>()
                .into(),
            slab_buckets: SlabAllocationBucketsPhys {
                buckets: self
                    .slab_buckets
                    .0
                    .iter()
                    .map(|(&bucket_size, bucket)| (bucket_size, bucket.is_extent_based))
                    .collect(),
            },
        };
        debug!(
            "flushed BlockAllocator in {}ms",
            begin.elapsed().as_millis()
        );
        phys
    }

    pub fn available(&self) -> u64 {
        self.available_space
    }

    pub fn freeing(&self) -> u64 {
        self.freeing_space
    }

    pub fn size(&self) -> u64 {
        self.capacity.iter().map(|(extent, _)| extent.size).sum()
    }

    fn slab_id_from_extent_impl(
        capacity: &BiBTreeMap<Extent, SlabId>,
        slab_size: u64,
        extent: Extent,
    ) -> SlabId {
        let (capacity_extent, &capacity_slab) = capacity
            .left_range((Unbounded, Included(extent.location)))
            .next_back()
            .unwrap();

        assert!(capacity_extent.contains(&extent));
        capacity_slab + ((extent.location - capacity_extent.location) / slab_size)
    }

    fn slab_id_from_extent(&self, extent: Extent) -> SlabId {
        let slab_size64 = u64::from(self.slab_size);
        assert_le!(extent.size, slab_size64);

        let slab_id = BlockAllocator::slab_id_from_extent_impl(&self.capacity, slab_size64, extent);

        assert_lt!(slab_id.0, self.slabs.0.len() as u64);
        assert!(self.slab_extent_from_id(slab_id).contains(&extent));

        slab_id
    }

    fn slab_extent_from_id(&self, slab_id: SlabId) -> Extent {
        let (containing_extent, &extent_slab) = self
            .capacity
            .right_range((Unbounded, Included(slab_id)))
            .next_back()
            .unwrap();
        containing_extent.range(
            (slab_id - extent_slab) * u64::from(self.slab_size),
            self.slab_size.into(),
        )
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
enum SlabPhysType {
    BitmapBased { block_size: u32 },
    ExtentBased { max_size: u32 },
    Free,
    Evacuating,
}
impl OnDisk for SlabPhysType {}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SlabPhys {
    generation: SlabGeneration,
    slab_type: SlabPhysType,
}
impl OnDisk for SlabPhys {}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SlabAllocationBucketsPhys {
    // Buckets sorted by max-allocation-size (ascending-order)
    // (max allocation size, is extent based)
    buckets: Vec<(u32, bool)>,
}
impl OnDisk for SlabAllocationBucketsPhys {}

impl SlabAllocationBucketsPhys {
    fn default() -> Self {
        let mut buckets = Vec::new();
        // Create the first few buckets for bitmap-based slab use
        for b in 1..(16 * 1024 / 512) {
            buckets.push((b * 512, false));
        }
        // Create a few more extent-based buckets for larger sizes
        buckets.push((64 * 1024, true));
        buckets.push((256 * 1024, true));
        buckets.push((1024 * 1024, true));
        buckets.push((*DEFAULT_SLAB_SIZE, true));

        SlabAllocationBucketsPhys { buckets }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockAllocatorPhys {
    slab_size: u32,

    spacemap: SpaceMapPhys,
    spacemap_next: SpaceMapPhys,
    next_slab_to_condense: SlabId,

    capacity: Vec<Extent>,

    // TODO: if this is too big to be writing every checkpoint,
    //       we could use a BlockBasedLog<(SlabId, SlabPhysType)>
    // Note: slabs are located within the `capacity` in the order given
    slabs: TerseVec<SlabPhys>,
    slab_buckets: SlabAllocationBucketsPhys,
}
impl OnDisk for BlockAllocatorPhys {}

impl BlockAllocatorPhys {
    pub fn new<T>(capacity: T) -> BlockAllocatorPhys
    where
        T: IntoIterator<Item = Extent>,
    {
        let mut this = BlockAllocatorPhys {
            slab_size: *DEFAULT_SLAB_SIZE,
            spacemap: SpaceMapPhys::new(),
            spacemap_next: SpaceMapPhys::new(),
            next_slab_to_condense: SlabId(0),
            capacity: Default::default(),
            slabs: TerseVec(Vec::new()),
            slab_buckets: DEFAULT_SLAB_BUCKETS.clone(),
        };
        this.extend(capacity);
        this
    }

    /// Add new capacity
    pub fn extend<T>(&mut self, capacity: T)
    where
        T: IntoIterator<Item = Extent>,
    {
        let slabsize = u64::from(self.slab_size);
        for extent in capacity {
            let nslabs = extent.size / slabsize;
            self.slabs.0.extend(
                iter::repeat(SlabPhys {
                    generation: SlabGeneration(0),
                    slab_type: SlabPhysType::Free,
                })
                .take(usize::from64(nslabs)),
            );
            // capacity is aligned to be a multiple of slabsize
            self.capacity.push(extent.range(0, nslabs * slabsize));
        }
    }

    pub fn claim(&self, builder: &mut ExtentAllocatorBuilder) {
        self.spacemap.claim(builder);
        self.spacemap_next.claim(builder);
    }

    pub fn capacity(&self) -> Vec<Extent> {
        self.capacity.clone()
    }

    pub fn spacemap_bytes(&self) -> u64 {
        self.spacemap.bytes()
    }

    pub fn spacemap_next_bytes(&self) -> u64 {
        self.spacemap_next.bytes()
    }

    pub fn spacemap_capacity_bytes(&self) -> u64 {
        self.spacemap.capacity_bytes()
    }

    pub fn spacemap_next_capacity_bytes(&self) -> u64 {
        self.spacemap_next.capacity_bytes()
    }
}

pub async fn zcachedb_dump_spacemaps(
    phys: BlockAllocatorPhys,
    block_access: Arc<BlockAccess>,
    extent_allocator: Arc<ExtentAllocator>,
) {
    println!("DUMP SPACEMAP");
    println!("{:?}", phys.spacemap);
    let spacemap = SpaceMap::open(
        block_access.clone(),
        extent_allocator.clone(),
        phys.spacemap,
    );
    spacemap.load(|entry| println!("{:?}", entry)).await;
    println!();

    println!("DUMP SPACEMAP_NEXT");
    println!("{:?}", phys.spacemap_next);
    let spacemap_next = SpaceMap::open(
        block_access.clone(),
        extent_allocator.clone(),
        phys.spacemap_next,
    );
    spacemap_next.load(|entry| println!("{:?}", entry)).await;
}

async fn zcachedb_load_slab_state(
    block_access: Arc<BlockAccess>,
    extent_allocator: Arc<ExtentAllocator>,
    phys: BlockAllocatorPhys,
) -> Slabs {
    let spacemap = SpaceMap::open(
        block_access.clone(),
        extent_allocator.clone(),
        phys.spacemap,
    );
    let spacemap_next = SpaceMap::open(
        block_access.clone(),
        extent_allocator.clone(),
        phys.spacemap_next,
    );
    let slab_size = phys.slab_size;
    let capacity: BiBTreeMap<Extent, SlabId> = {
        let mut id = SlabId(0);
        phys.capacity
            .into_iter()
            .map(|extent| {
                let start = id;
                id = id + extent.size / u64::from(slab_size);
                (extent, start)
            })
            .collect()
    };
    Slabs::open(&capacity, &spacemap, &spacemap_next, slab_size, &phys.slabs).await
}

struct AllocationBucketStatistics {
    pub nslabs: u64,
    pub free_space: u64,
    pub slab_size: u64,
    pub total_segments: u64,
}

impl AllocationBucketStatistics {
    fn new(slab_size: u64) -> AllocationBucketStatistics {
        AllocationBucketStatistics {
            nslabs: 0,
            free_space: 0,
            slab_size,
            total_segments: 0,
        }
    }

    fn add_slab(&mut self, slab: &Slab) {
        self.total_segments += slab.num_segments();
        self.free_space += slab.free_space();
        self.nslabs += 1;
    }

    fn total_space(&self) -> u64 {
        self.slab_size * self.nslabs
    }

    fn allocated_space(&self) -> u64 {
        self.total_space() - self.free_space
    }

    fn capacity_perc(&self) -> u64 {
        (self.allocated_space() * 100) / self.total_space()
    }

    fn segments_per_slab(&self) -> u64 {
        self.total_segments / self.nslabs
    }

    fn stackgraph(&self, hist_scaling_factor: u64) -> String {
        if self.nslabs == 0 {
            return "".to_string();
        }
        let hist_factor = cmp::max(usize::from64(hist_scaling_factor), REPORT_HISTOGRAM_WIDTH);
        let hist_slots = (usize::from64(self.nslabs) * REPORT_HISTOGRAM_WIDTH) / hist_factor;
        let free_slots = ((usize::from64(self.free_space) * hist_slots)
            + usize::from64(self.total_space() / 2 - 1))
            / usize::from64(self.total_space());

        format!(
            "{}{}",
            "*".repeat(hist_slots - free_slots),
            "=".repeat(free_slots),
        )
    }
}

impl fmt::Display for AllocationBucketStatistics {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:>6} {:>7} {:>7} {:>7} {:>3}% {:>6}",
            self.nslabs,
            nice_p2size(self.total_space()),
            nice_p2size(self.allocated_space()),
            nice_p2size(self.free_space),
            if self.nslabs != 0 {
                self.capacity_perc().to_string()
            } else {
                "-".to_string()
            },
            if self.nslabs != 0 {
                self.segments_per_slab().to_string()
            } else {
                "-".to_string()
            },
        )
    }
}

struct AllocationBucketInfo {
    extent_based: bool,
    max_size: u32,
    stats: AllocationBucketStatistics,
    slabs_by_freeness: BTreeSet<(u64, SlabId)>,
}

impl AllocationBucketInfo {
    fn new(extent_based: bool, max_size: u32, slab_size: u64) -> AllocationBucketInfo {
        AllocationBucketInfo {
            extent_based,
            stats: AllocationBucketStatistics::new(slab_size),
            max_size,
            slabs_by_freeness: BTreeSet::default(),
        }
    }

    fn add_slab(&mut self, slab: &Slab) {
        self.stats.add_slab(slab);
        self.slabs_by_freeness.insert((slab.free_space(), slab.id));
    }

    // Given a permille value (1000-quantile) for the number of slabs in this
    // bucket, this function returns the following tuple (allocated_bytes of
    // those slabs, capacity ratio [e.g. allocated over total space] of those
    // slabs)
    fn allocated_quantile(&self, permille: u64) -> (u64, f64) {
        let mut allocated_bytes = 0;
        let slabs_to_visit = cmp::max(
            (usize::from64(permille) * self.slabs_by_freeness.len()) / 1000,
            1,
        );
        for (free_space, _) in self.slabs_by_freeness.iter().rev().take(slabs_to_visit) {
            allocated_bytes += self.stats.slab_size - free_space;
        }
        let total_space = (slabs_to_visit as u64) * self.stats.slab_size;
        (
            allocated_bytes,
            (allocated_bytes as f64) / total_space as f64,
        )
    }
}

impl fmt::Display for AllocationBucketInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} {:>8}: {}",
            if self.extent_based { "*" } else { " " },
            nice_p2size(u64::from(self.max_size)),
            self.stats
        )
    }
}

struct SlabBucketsReport {
    buckets: BTreeMap<u32, AllocationBucketInfo>,
    total: AllocationBucketStatistics,
    hist_scaling_factor: u64,
}

const REPORT_HISTOGRAM_WIDTH: usize = 39;

impl SlabBucketsReport {
    fn new(buckets: &[(u32, bool)], slab_size: u64) -> SlabBucketsReport {
        SlabBucketsReport {
            buckets: buckets
                .iter()
                .map(|(max_size, is_extent_based)| {
                    (
                        *max_size,
                        AllocationBucketInfo::new(*is_extent_based, *max_size, slab_size),
                    )
                })
                .collect(),
            total: AllocationBucketStatistics::new(slab_size),
            hist_scaling_factor: 0,
        }
    }

    fn reset_hist_scaling_factor(&mut self, count: u64) {
        self.hist_scaling_factor = cmp::max(self.hist_scaling_factor, count);
    }

    fn add_slab(&mut self, slab: &Slab) {
        // Free and Evacuating slabs don't belong on a bucket, just log them for the total stats
        match slab.info {
            SlabType::BitmapBased(_) | SlabType::ExtentBased(_) => {
                let bucket_info = self.buckets.range_mut(slab.max_size()..).next().unwrap().1;
                bucket_info.add_slab(slab);
                let nslabs = bucket_info.stats.nslabs;
                self.reset_hist_scaling_factor(nslabs);
            }
            SlabType::Free(_) | SlabType::Evacuating(_) => {}
        }
        self.total.add_slab(slab);
    }

    fn dump_report(&self, verbosity: u64) {
        println!("E MAX_SIZE:  NSLAB    SIZE   ALLOC    FREE  CAP  SEG/S NSLAB");
        for bucket in self.buckets.values() {
            println!(
                "{} {}",
                bucket,
                bucket.stats.stackgraph(self.hist_scaling_factor)
            );

            if verbosity > 0 && bucket.stats.nslabs > 0 {
                const CAP_BUCKET_PERCENTAGE_RANGE: usize = 10;
                let mut cap_hist = [0u64; CAP_BUCKET_PERCENTAGE_RANGE];
                let mut max_count = 0;
                for (slab_free_space, _) in bucket.slabs_by_freeness.iter() {
                    let perc_cap = usize::from64(
                        ((bucket.stats.slab_size - slab_free_space) * 100) / bucket.stats.slab_size,
                    );
                    let idx = if perc_cap == 100 {
                        9
                    } else {
                        perc_cap / CAP_BUCKET_PERCENTAGE_RANGE
                    };
                    cap_hist[idx] += 1;
                    max_count = cmp::max(max_count, usize::from64(cap_hist[idx]));
                }
                max_count = cmp::max(max_count, REPORT_HISTOGRAM_WIDTH);

                let (perm_1_bytes, perm_1_perc) = bucket.allocated_quantile(1);
                let (perm_10_bytes, perm_10_perc) = bucket.allocated_quantile(10);
                let (perm_100_bytes, perm_100_perc) = bucket.allocated_quantile(100);

                println!("\t%CAP: NSLABS");
                for (idx, count) in cap_hist.iter().enumerate() {
                    println!(
                        "\t{:>4}: {} {}",
                        idx * CAP_BUCKET_PERCENTAGE_RANGE,
                        "*".repeat((usize::from64(*count) * REPORT_HISTOGRAM_WIDTH) / max_count),
                        *count
                    );
                }
                println!("\t-------------");
                println!(
                    "\tallocated space in 0.1% of free-est slabs: {} ({:.1}%)",
                    nice_p2size(perm_1_bytes),
                    perm_1_perc * 100.0
                );
                println!(
                    "\tallocated space in   1% of free-est slabs: {} ({:.1}%)",
                    nice_p2size(perm_10_bytes),
                    perm_10_perc * 100.0
                );
                println!(
                    "\tallocated space in  10% of free-est slabs: {} ({:.1}%)",
                    nice_p2size(perm_100_bytes),
                    perm_100_perc * 100.0
                );
                println!("\t-------------");
            }
        }
    }
}

fn zcachedb_dump_slabs_print_legend() {
    println!("E: Extent-based");
    println!("MAX_SIZE: largest allocation that can be made to these slabs");
    println!("NSLAB: number of slabs");
    println!("SIZE: total bytes in slabs (ALLOC + FREE)");
    println!("ALLOC: allocated bytes in slabs");
    println!("FREE: free (available) bytes in slabs");
    println!("CAP: percent allocated (ALLOC / SIZE)");
    println!("SEG/S: average number of disjoint free segments per slab");
    println!();
}

pub async fn zcachedb_dump_slabs(
    block_access: Arc<BlockAccess>,
    extent_allocator: Arc<ExtentAllocator>,
    phys: BlockAllocatorPhys,
    opts: DumpSlabsOptions,
) {
    let slab_size = u64::from(phys.slab_size);

    let mut buckets_by_max_size = SlabBucketsReport::new(&phys.slab_buckets.buckets, slab_size);
    let bitmap_summary_dist: Vec<(u32, bool)> = [1, 2, 4, 8, 16]
        .iter()
        .map(|kbytes| (kbytes * 1024u32, false))
        .collect();
    let mut bitmap_based_summary = SlabBucketsReport::new(&bitmap_summary_dist, slab_size);
    let extent_summary_dist: Vec<(u32, bool)> = [64, 256, 1024, 16384]
        .iter()
        .map(|kbytes| (kbytes * 1024u32, true))
        .collect();
    let mut extent_based_summary = SlabBucketsReport::new(&extent_summary_dist, slab_size);
    let mut empty_total = AllocationBucketStatistics::new(slab_size);
    let mut evacuating_total = AllocationBucketStatistics::new(slab_size);

    let slabs = zcachedb_load_slab_state(block_access, extent_allocator, phys).await;

    for slab in slabs.0.iter() {
        if opts.verbosity > 1 {
            slab.dump_info();
        }
        buckets_by_max_size.add_slab(slab);

        match &slab.info {
            SlabType::BitmapBased(_) => bitmap_based_summary.add_slab(slab),
            SlabType::ExtentBased(_) => extent_based_summary.add_slab(slab),
            SlabType::Free(_) => empty_total.add_slab(slab),
            SlabType::Evacuating(_) => evacuating_total.add_slab(slab),
        }
    }
    let max_scaling_factor = cmp::max(
        bitmap_based_summary.hist_scaling_factor,
        extent_based_summary.hist_scaling_factor,
    );
    bitmap_based_summary.reset_hist_scaling_factor(max_scaling_factor);
    extent_based_summary.reset_hist_scaling_factor(max_scaling_factor);
    buckets_by_max_size.reset_hist_scaling_factor(max_scaling_factor);

    println!("============================================================");
    zcachedb_dump_slabs_print_legend();
    buckets_by_max_size.dump_report(opts.verbosity);
    println!();
    println!("============================================================");
    println!("========================  SUMMARY  =========================");
    println!("============================================================");
    println!();
    bitmap_based_summary.dump_report(opts.verbosity);
    println!("------------------------------------------------------------");
    println!("    BITMAP: {}", bitmap_based_summary.total);
    println!();
    extent_based_summary.dump_report(opts.verbosity);
    println!("------------------------------------------------------------");
    println!("    EXTENT: {}", extent_based_summary.total);
    println!("------------------------------------------------------------");
    println!("     EMPTY: {}", empty_total);
    println!("------------------------------------------------------------");
    println!("EVACUATING: {}", evacuating_total);
    println!("============================================================");
    println!("E MAX_SIZE:  NSLAB    SIZE   ALLOC    FREE  CAP  SEG/S NSLAB");
    println!("     TOTAL: {}", buckets_by_max_size.total);
    println!();
}
