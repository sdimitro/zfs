use crate::base_types::*;
use crate::base_types::{Extent, OnDisk};
use crate::block_access::BlockAccess;
use crate::extent_allocator::ExtentAllocator;
use crate::get_tunable;
use crate::range_tree::RangeTree;
use crate::space_map::SpaceMap;
use crate::space_map::SpaceMapPhys;
use crate::zettacache::DEFAULT_SLAB_SIZE;
use lazy_static::lazy_static;
use log::debug;
use more_asserts::*;
use num::{range, Num, NumCast};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::iter;
use std::ops::Bound::*;
use std::sync::Arc;

lazy_static! {
    static ref BUCKETS: SlabAllocationBucketsPhys =
        get_tunable("buckets", SlabAllocationBucketsPhys::default());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SlabId(u64);

trait SlabTrait {
    fn import_alloc(&mut self, extent: Extent);
    fn import_free(&mut self, extent: Extent);
    fn allocate(&mut self, size: u64) -> Option<Extent>;
    fn free(&mut self, extent: Extent);
    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap);
    fn get_max_size(&self) -> u32;
    fn get_free_space(&self) -> u64;
    fn get_allocated_space(&self) -> u64;
    fn get_phys(&self) -> SlabPhys;
}

struct BitmapSlab {
    allocatable: RoaringBitmap,
    allocating: RoaringBitmap,
    freeing: RoaringBitmap,

    total_slots: u32,
    allocatable_slots: u32,
    slot_size: u32,
    slab_offset: u64,
}

impl BitmapSlab {
    fn new_slab(id: SlabId, slab_offset: u64, slab_size: u32, block_size: u32) -> Slab {
        let free_slots = slab_size / block_size;
        let mut allocatable = RoaringBitmap::new();

        allocatable.insert_range(0..free_slots as u64);
        assert_eq!(allocatable.len() as u32, free_slots);

        Slab::new(
            id,
            SlabType::BitmapBased(BitmapSlab {
                allocatable,
                allocating: Default::default(),
                freeing: Default::default(),
                total_slots: free_slots,
                allocatable_slots: free_slots,
                slot_size: block_size,
                slab_offset,
            }),
        )
    }

    fn verify_slab_extent(&self, extent: Extent) {
        assert_le!(extent.size, self.get_max_size() as usize);
        assert_ge!(extent.location.offset, self.slab_offset);
        assert_le!(
            extent.location.offset + extent.size as u64,
            self.slab_offset + (self.total_slots * self.slot_size) as u64
        );
    }
}

impl SlabTrait for BitmapSlab {
    fn import_alloc(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);

        let internal_offset = extent.location.offset - self.slab_offset;
        assert_eq!(internal_offset % self.slot_size as u64, 0);

        let slot = (internal_offset as u32) / self.slot_size;
        assert_lt!(slot, self.total_slots);
        self.allocatable.remove(slot);

        self.allocatable_slots -= 1;
    }

    fn import_free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);

        let internal_offset = extent.location.offset - self.slab_offset;
        assert_eq!(internal_offset % self.slot_size as u64, 0);

        let slot = (extent.location.offset as u32) / self.slot_size;
        assert_lt!(slot, self.total_slots);
        self.allocatable.insert(slot);

        assert_lt!(self.allocatable_slots, self.total_slots);
        self.allocatable_slots += 1;
    }

    fn allocate(&mut self, size: u64) -> Option<Extent> {
        assert_ge!(self.slot_size as u64, size);
        if self.allocatable_slots == 0 {
            return None;
        }

        let slot = self.allocatable.min().unwrap();
        self.allocating.insert(slot);
        self.allocatable.remove(slot);
        self.allocatable_slots -= 1;

        Some(Extent {
            location: DiskLocation {
                offset: (slot * self.slot_size) as u64 + self.slab_offset,
            },
            size: self.slot_size as usize,
        })
    }

    fn free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);

        let internal_offset = extent.location.offset - self.slab_offset;
        assert_eq!(internal_offset % self.slot_size as u64, 0);

        let slot = (internal_offset as u32) / self.slot_size;
        assert!(
            !self.allocatable.contains(slot),
            "double free at slot {:?}",
            slot
        );

        self.freeing.insert(slot);
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        for slot in self.freeing.iter() {
            spacemap.free(
                (slot * self.slot_size) as u64 + self.slab_offset,
                self.slot_size as u64,
            );
            self.allocatable.insert(slot);
            self.allocatable_slots += 1;
        }
        self.freeing.clear();

        for slot in self.allocating.iter() {
            spacemap.alloc(
                (slot * self.slot_size) as u64 + self.slab_offset,
                self.slot_size as u64,
            );
        }
        self.allocating.clear();
    }

    fn get_max_size(&self) -> u32 {
        self.slot_size
    }

    fn get_free_space(&self) -> u64 {
        (self.allocatable_slots * self.slot_size) as u64
    }

    fn get_allocated_space(&self) -> u64 {
        ((self.total_slots - self.allocatable_slots) * self.slot_size) as u64
    }

    fn get_phys(&self) -> SlabPhys {
        SlabPhys::BitmapBased {
            block_size: self.slot_size,
        }
    }
}

struct ExtentSlab {
    allocatable: RangeTree,
    allocating: RangeTree,
    freeing: RangeTree,
    last_location: u64,

    total_space: u64,
    max_allowed_alloc_size: u32,
    slab_offset: u64,
}

impl ExtentSlab {
    fn new_slab(id: SlabId, slab_offset: u64, slab_size: u32, max_allowed_alloc_size: u32) -> Slab {
        let mut allocatable: RangeTree = Default::default();
        allocatable.add(slab_offset, slab_size.into());
        Slab::new(
            id,
            SlabType::ExtentBased(ExtentSlab {
                allocatable,
                allocating: Default::default(),
                freeing: Default::default(),
                last_location: 0,
                total_space: slab_size as u64,
                max_allowed_alloc_size,
                slab_offset,
            }),
        )
    }

    fn verify_slab_extent(&self, extent: Extent) {
        assert_le!(extent.size, self.get_max_size() as usize);
        assert_ge!(extent.location.offset, self.slab_offset);
        assert_le!(
            extent.location.offset + extent.size as u64,
            self.slab_offset + self.total_space
        );
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
}

impl SlabTrait for ExtentSlab {
    fn import_alloc(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);
        self.allocatable
            .remove(extent.location.offset, extent.size as u64);
    }

    fn import_free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);
        self.allocatable
            .add(extent.location.offset, extent.size as u64);
    }

    fn allocate(&mut self, size: u64) -> Option<Extent> {
        assert_le!(size, self.get_max_size() as u64);
        // find next segment where this fits, or largest free segment.
        // XXX keep size-sorted tree as well?
        match self.allocate_impl(size, self.last_location, u64::MAX) {
            Some(e) => Some(e),
            None => self.allocate_impl(size, 0, self.last_location),
        }
    }

    fn free(&mut self, extent: Extent) {
        self.verify_slab_extent(extent);
        self.allocatable
            .verify_absent(extent.location.offset, extent.size as u64);
        self.allocating
            .verify_absent(extent.location.offset, extent.size as u64);
        self.freeing.add(extent.location.offset, extent.size as u64);
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        self.freeing.verify_space();
        self.allocating.verify_space();
        self.allocatable.verify_space();

        // Space freed during this checkpoint is now available for reallocation.
        for (&start, &size) in self.freeing.iter() {
            self.allocating.verify_absent(start, size);
            spacemap.free(start, size);
            self.allocatable.add(start, size);
        }
        self.freeing.clear();

        for (&start, &size) in self.allocating.iter() {
            self.allocatable.verify_absent(start, size);
            spacemap.alloc(start, size);
        }
        self.allocating.clear();
    }

    fn get_max_size(&self) -> u32 {
        self.max_allowed_alloc_size
    }

    fn get_free_space(&self) -> u64 {
        self.allocatable.space()
    }

    fn get_allocated_space(&self) -> u64 {
        self.total_space - self.get_free_space()
    }

    fn get_phys(&self) -> SlabPhys {
        SlabPhys::ExtentBased {
            max_size: self.max_allowed_alloc_size,
        }
    }
}

struct FreeSlab {
    // When importing a pool and going through its spacemap log we
    // may find allocation and fress for slabs that are currently
    // FreeSlabs. Keep a space counter for each of those FreeSlabs
    // and make sure that the space allocated for each of them
    // after import is 0.
    space_after_import: u64,
}

impl FreeSlab {
    fn new_slab(id: SlabId) -> Slab {
        Slab::new(
            id,
            SlabType::Free(FreeSlab {
                space_after_import: 0,
            }),
        )
    }
}

impl SlabTrait for FreeSlab {
    fn import_alloc(&mut self, extent: Extent) {
        self.space_after_import += extent.size as u64;
    }

    fn import_free(&mut self, extent: Extent) {
        self.space_after_import -= extent.size as u64;
    }

    fn allocate(&mut self, size: u64) -> Option<Extent> {
        panic!(
            "attempting to allocate block from free slab: size = {}",
            size
        );
    }

    fn free(&mut self, extent: Extent) {
        panic!("attempting to free block from free slab: {:?}", extent);
    }

    fn flush_to_spacemap(&mut self, _spacemap: &mut SpaceMap) {
        panic!("attempting to flush free slab",);
    }

    fn get_max_size(&self) -> u32 {
        panic!("free slab doesn't have a maximum allocation size");
    }

    fn get_free_space(&self) -> u64 {
        panic!("free slab doesn't have free space");
    }

    fn get_allocated_space(&self) -> u64 {
        panic!("free slab doesn't have allocated space");
    }

    fn get_phys(&self) -> SlabPhys {
        SlabPhys::Free
    }
}

enum SlabType {
    BitmapBased(BitmapSlab),
    ExtentBased(ExtentSlab),
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
            SlabType::Free(t) => cb(t),
        }
    }
}

struct Slab {
    id: SlabId,
    info: SlabType,
    is_dirty: bool,
}

impl Slab {
    fn new(id: SlabId, info: SlabType) -> Slab {
        Slab {
            id,
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

    fn allocate(&mut self, size: u64) -> Option<Extent> {
        self.info.with_trait_mut(|t| t.allocate(size))
    }

    fn free(&mut self, extent: Extent) {
        self.info.with_trait_mut(|t| t.free(extent));
    }

    fn flush_to_spacemap(&mut self, spacemap: &mut SpaceMap) {
        self.info.with_trait_mut(|t| t.flush_to_spacemap(spacemap));
        self.is_dirty = false;
    }

    fn get_max_size(&self) -> u32 {
        self.info.with_trait(|t| t.get_max_size())
    }

    fn get_free_space(&self) -> u64 {
        self.info.with_trait(|t| t.get_free_space())
    }

    fn get_allocated_space(&self) -> u64 {
        self.info.with_trait(|t| t.get_allocated_space())
    }

    fn get_phys(&self) -> SlabPhys {
        self.info.with_trait(|t| t.get_phys())
    }

    fn to_sorted_slab_entry(&self) -> SortedSlabEntry {
        SortedSlabEntry {
            allocated_space: self.get_allocated_space(),
            slab_id: self.id,
        }
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
        let last_allocated = by_freeness.iter().next().map(|entry| *entry);
        SortedSlabs {
            is_extent_based,
            by_freeness,
            last_allocated,
            been_through_once: false,
        }
    }

    fn get_current(&self) -> Option<SlabId> {
        self.last_allocated.map(|la| la.slab_id)
    }

    fn advance(&mut self) -> Option<SlabId> {
        if self.been_through_once {
            // If this clause is hit it means that we've been through all
            // the slabs in this SortedSlab set and we've also filled up
            // a slab that we just created and inserted to the set. In
            // order to not iterate through all the slabs again for this
            // checkpoint we last_allocated to None and return that.
            self.last_allocated = None;
            return self.get_current();
        }

        if let Some(last_allocated) = self.last_allocated {
            self.last_allocated = self
                .by_freeness
                .range((Excluded(last_allocated), Unbounded))
                .next()
                .map(|entry| *entry);
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

    fn insert(&mut self, sorted_slab_entry: SortedSlabEntry) {
        self.by_freeness.insert(sorted_slab_entry);
        self.last_allocated = Some(sorted_slab_entry);
    }
}

struct SlabAllocationBuckets {
    // buckets:
    // key - max allocation that this set of slabs can satisfy
    // value - the set of sorted slabs
    //
    // Note: Even though not strictly necessary in general
    // the BitmapBased slabs are used before all the ExtentBased
    // ones (i.e. Bitmaps are used for smaller allocation sizes).
    buckets: BTreeMap<u32, SortedSlabs>,
}

impl SlabAllocationBuckets {
    fn new(slab_buckets: SlabAllocationBucketsPhys) -> Self {
        let mut buckets = BTreeMap::new();
        for t in slab_buckets.buckets {
            buckets.insert(t.0, SortedSlabs::new(t.1, iter::empty()));
        }
        SlabAllocationBuckets { buckets }
    }

    fn add_slab_to_bucket(&mut self, bucket: u32, slab: &Slab) {
        self.buckets
            .get_mut(&bucket)
            .unwrap()
            .insert(slab.to_sorted_slab_entry());
    }

    fn get_bucket_for_allocation_size(&mut self, request_size: u64) -> (&u32, &mut SortedSlabs) {
        assert_le!(request_size, *DEFAULT_SLAB_SIZE as u64);
        self.buckets
            .range_mut((Included(request_size as u32), Unbounded))
            .next()
            .unwrap()
    }
}

struct Slabs(Vec<Slab>);

impl Slabs {
    fn get(&self, id: SlabId) -> &Slab {
        &self.0[id.0 as usize]
    }

    fn get_mut(&mut self, id: SlabId) -> &mut Slab {
        &mut self.0[id.0 as usize]
    }
}

pub struct BlockAllocator {
    coverage: Extent,
    slab_size: u32,

    spacemap: SpaceMap,
    slabs: Slabs,
    dirty_slabs: Vec<SlabId>,
    free_slabs: Vec<SlabId>,

    slab_buckets: SlabAllocationBuckets,

    available_space: u64,
    freeing_space: u64,

    // XXX: Currently the block allocator assumes that requested allocation sizes
    //      are aligned to sector size from higher up in the call hierarchy so the
    //      field below is only used for assertions.
    sector_size: usize,
}

impl BlockAllocator {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: BlockAllocatorPhys,
    ) -> BlockAllocator {
        let spacemap = SpaceMap::open(
            block_access.clone(),
            extent_allocator.clone(),
            phys.spacemap,
        );

        let coverage = spacemap.get_coverage();
        let slab_size = phys.slab_size;
        let num_slabs = coverage.size / (slab_size as usize);
        assert_eq!(num_slabs, phys.slabs.len());
        let mut available_space = 0;

        let mut slabs = Vec::with_capacity(num_slabs);
        for (slab_id, phys_slab) in phys.slabs.iter().enumerate() {
            // ensure that we are pushing at the right offset of the Vec
            assert_eq!(slab_id, slabs.len());

            let sid = SlabId(slab_id as u64);
            let slab_offset = coverage.location.offset + (slab_size as u64 * num_slabs as u64)
                - (sid.0 + 1) * (slab_size as u64);
            match phys_slab {
                SlabPhys::BitmapBased { block_size } => {
                    slabs.push(BitmapSlab::new_slab(
                        sid,
                        slab_offset,
                        phys.slab_size,
                        *block_size,
                    ));
                }
                SlabPhys::ExtentBased { max_size } => {
                    slabs.push(ExtentSlab::new_slab(
                        sid,
                        slab_offset,
                        phys.slab_size,
                        *max_size,
                    ));
                }
                SlabPhys::Free => {
                    slabs.push(FreeSlab::new_slab(sid));
                }
            }
        }

        spacemap
            .load(|offset, size, is_alloc| {
                let offset_from_end = coverage.location.offset + (coverage.size as u64) - offset;
                let slab_id = SlabId(offset_from_end / slab_size as u64);

                let extent = Extent {
                    location: DiskLocation { offset },
                    size: size as usize,
                };

                if is_alloc {
                    slabs[slab_id.0 as usize].import_alloc(extent)
                } else {
                    slabs[slab_id.0 as usize].import_free(extent)
                }
            })
            .await;

        let mut free_slabs = Vec::new();
        let mut slab_buckets = SlabAllocationBuckets::new(phys.slab_buckets);
        for slab in slabs.iter() {
            match &slab.info {
                SlabType::BitmapBased(_) | SlabType::ExtentBased(_) => {
                    slab_buckets.add_slab_to_bucket(slab.get_max_size(), slab);
                    available_space += slab.get_free_space();
                }
                SlabType::Free(info) => {
                    // Ensure that we have no leftover allocated space in
                    // the FreeSlab after reading the whole spacemap.
                    assert_eq!(info.space_after_import, 0);
                    free_slabs.push(slab.id);
                    available_space += slab_size as u64;
                }
            }
        }

        BlockAllocator {
            coverage,
            slab_size,
            spacemap,
            slabs: Slabs(slabs),
            dirty_slabs: Default::default(),
            free_slabs,
            slab_buckets,
            available_space,
            freeing_space: 0,
            sector_size: block_access.sector_size(),
        }
    }

    fn dirty_slab_id(&mut self, slab_id: SlabId) {
        let slab = &mut self.slabs.get_mut(slab_id);
        if !slab.is_dirty {
            self.dirty_slabs.push(slab_id);
            slab.is_dirty = true;
        }
    }

    fn allocate_from_new_slab(&mut self, request_size: u64) -> Option<Extent> {
        let slab_size = self.slab_size;
        let new_id = match self.free_slabs.pop() {
            Some(id) => id,
            None => {
                debug!("SLAB-ALLOCATOR: all-out-of-slabs");
                return None;
            }
        };
        let slab_offset = self.slab_offset_from_slab_id(new_id);

        let (&max_allocation_size, sorted_slabs) = self
            .slab_buckets
            .get_bucket_for_allocation_size(request_size);

        let mut new_slab = if sorted_slabs.is_extent_based {
            ExtentSlab::new_slab(new_id, slab_offset, slab_size, max_allocation_size)
        } else {
            BitmapSlab::new_slab(new_id, slab_offset, slab_size, max_allocation_size)
        };
        sorted_slabs.insert(new_slab.to_sorted_slab_entry());

        let extent = new_slab.allocate(request_size);
        assert!(extent.is_some());
        assert!(matches!(self.slabs.get(new_id).info, SlabType::Free(_)));
        *self.slabs.get_mut(new_id) = new_slab;
        self.dirty_slab_id(new_id);
        debug!(
            "SLAB-ALLOCATOR: satisfied-allocation-new-slab: {:?}",
            extent
        );
        self.available_space -= extent.unwrap().size as u64;
        extent
    }

    pub fn allocate(&mut self, request_size: u64) -> Option<Extent> {
        assert_ge!(self.slab_size as u64, request_size);
        debug!("SLAB-ALLOCATOR: allocate: {:?}", request_size);

        // Note: we assume allocation sizes are guaranteed to be aligned
        // from the caller for now.
        assert_eq!(request_size, self.round_up_to_sector(request_size));

        let sorted_slabs = self
            .slab_buckets
            .get_bucket_for_allocation_size(request_size)
            .1;

        // XXX - WIP Allocation Algorithm
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
                        debug!("SLAB-ALLOCATOR: satisfied-allocation: {:?}", extent);
                        self.dirty_slab_id(id);
                        self.available_space -= extent.size as u64;
                        return Some(extent);
                    }
                    None => {
                        let debug = sorted_slabs.advance();
                        debug!("SLAB-ALLOCATOR: advance: {:?}", debug);
                    }
                },
                None => return self.allocate_from_new_slab(request_size),
            }
        }
    }

    pub fn free(&mut self, extent: Extent) {
        assert_eq!(extent.size, self.round_up_to_sector(extent.size));
        debug!("SLAB-ALLOCATOR: free: {:?}", extent);

        let slab_id = self.slab_id_from_extent(extent);
        self.slabs.get_mut(slab_id).free(extent);
        self.freeing_space += extent.size as u64;
        self.dirty_slab_id(slab_id);
    }

    pub async fn flush(&mut self) -> BlockAllocatorPhys {
        let mut dirty_buckets = HashSet::new();

        // Flush any dirty slabs. If any slabs is completely empty mark it as free.
        // Keep track of the buckets/SortedSlabs sets that these dirty slabs belong
        // to so later we can update their slab order by freeness.
        debug!(
            "BLOCK-ALLOCATOR: flushing-dirty-slabs: {}",
            self.dirty_slabs.len()
        );
        for slab_id in std::mem::take(&mut self.dirty_slabs) {
            let slab = self.slabs.get_mut(slab_id);
            slab.flush_to_spacemap(&mut self.spacemap);
            dirty_buckets.insert(slab.get_max_size());
            if slab.get_free_space() == (self.slab_size as u64) {
                self.free_slabs.push(slab.id);
                *slab = FreeSlab::new_slab(slab_id);
            }
        }

        debug!(
            "BLOCK-ALLOCATOR: resorting-allocation-buckets: {:?}",
            dirty_buckets
        );

        // Update any buckets which we've performed any allocations/frees before
        // before this checkpoint by recreating their SortedSlabs (which in turn
        // updates their order by freeness and also removes any empty slabs).
        for bucket_size in dirty_buckets {
            let bucket = self.slab_buckets.buckets.get_mut(&bucket_size).unwrap();
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

        let buckets_phys = self
            .slab_buckets
            .buckets
            .iter()
            .map(|(bucket_size, bucket)| (*bucket_size, bucket.is_extent_based))
            .collect();

        let slabs_phys = self.slabs.0.iter().map(|slab| slab.get_phys()).collect();

        BlockAllocatorPhys {
            slab_size: self.slab_size,
            slabs: slabs_phys,
            spacemap: self.spacemap.flush().await,
            slab_buckets: SlabAllocationBucketsPhys {
                buckets: buckets_phys,
            },
        }
    }

    pub fn get_available(&self) -> u64 {
        self.available_space
    }

    pub fn get_freeing(&self) -> u64 {
        self.freeing_space
    }

    //
    // |----------------| Device Offset 0
    // |... metadata ...|
    // |----------------| coverage.offset
    // |                |
    // |     ......     |  ....
    // |                |
    // |----------------|
    // |                | Slab n
    // |----------------|
    // |     ......     |  ....
    // |----------------|
    // |                | Slab 1
    // |----------------|
    // |                | Slab 0
    // |----------------| coverage.offset + (slab_size * slabs.len())
    // |     ......     |  ....
    //
    fn slab_id_from_extent(&self, extent: Extent) -> SlabId {
        let slab_sz = self.slab_size as u64;
        let num_slabs = self.slabs.0.len() as u64;
        assert_le!(extent.size as u64, slab_sz);

        let id =
            (num_slabs - 1) - ((extent.location.offset - self.coverage.location.offset) / slab_sz);
        assert_le!(id, num_slabs);

        let slab_id = SlabId(id);

        // check all boundaries now before proceeding
        let slab_offset = self.slab_offset_from_slab_id(slab_id);
        assert_ge!(extent.location.offset, slab_offset);
        assert_lt!(extent.location.offset, slab_offset + slab_sz);
        assert_le!(
            extent.location.offset + extent.size as u64,
            slab_offset + slab_sz
        );

        slab_id
    }

    fn slab_offset_from_slab_id(&self, slab_id: SlabId) -> u64 {
        let slab_sz = self.slab_size as u64;
        let num_slabs = self.slabs.0.len() as u64;
        self.coverage.location.offset + (slab_sz * num_slabs) - (slab_id.0 + 1) * slab_sz
    }

    pub fn round_up_to_sector<N: Num + NumCast + Copy>(&self, n: N) -> N {
        let sector_size: N = NumCast::from(self.sector_size).unwrap();
        (n + sector_size - N::one()) / sector_size * sector_size
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
enum SlabPhys {
    BitmapBased { block_size: u32 },
    ExtentBased { max_size: u32 },
    Free,
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
        for b in 0..(16 * 1024 / 512) {
            buckets.push((b * 512, false));
        }
        // Create a few more extent-based buckets for larger sizes
        buckets.push((64 * 1024, true));
        buckets.push((256 * 1024, true));
        buckets.push((1024 * 1024, true));
        buckets.push((*DEFAULT_SLAB_SIZE as u32, true));

        SlabAllocationBucketsPhys { buckets }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct BlockAllocatorPhys {
    slab_size: u32,
    // XXX if this is too big to be writing every checkpoint, we could use a BlockBasedLog<(SlabId, SlabPhysType)>
    slabs: Vec<SlabPhys>,
    spacemap: SpaceMapPhys,
    slab_buckets: SlabAllocationBucketsPhys,
}
impl OnDisk for BlockAllocatorPhys {}

impl BlockAllocatorPhys {
    // XXX eventually change this to indicate the size of each of the disks that we're managing
    pub fn new(offset: u64, size: u64) -> BlockAllocatorPhys {
        let slab_size = *DEFAULT_SLAB_SIZE as u32;
        let num_slabs = size / slab_size as u64;
        let mut slabs = Vec::new();
        for _ in range(0, num_slabs) {
            slabs.push(SlabPhys::Free);
        }

        // XXX: add the sizes here
        BlockAllocatorPhys {
            slab_size,
            slabs,
            spacemap: SpaceMapPhys::new(offset, size),
            slab_buckets: SlabAllocationBucketsPhys::default(),
        }
    }
}
