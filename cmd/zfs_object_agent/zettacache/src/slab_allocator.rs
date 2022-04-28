//! The disk is divided into equal-size slabs (by default, 16MB each).  The SlabAllocator knows
//! which slabs are allocated and which are free.  Other subsystems (e.g. BlockBasedLogs, the
//! BlockAllocator, and Checkpoints) can call into the SlabAllocator to allocate slabs for their
//! own use.
//!
//! By contrast, the SlabAllocatorPhys does not include information about which slabs are
//! allocated and which are freed.  When opening the Zettacache, before any slab allocations can
//! be performed, the other subsystems (e.g.  BBL, BlockAllocator, Checkpoint) must tell the Slab
//! Allocator which slabs are allocated, by calling SlabAllocatorBuilder::claim().

use std::cmp::max;
use std::collections::HashSet;
use std::ops::Add;
use std::ops::Bound::*;
use std::ops::Sub;
use std::sync::Mutex;

use bimap::BiBTreeMap;
use bytesize::ByteSize;
use more_asserts::*;
use rand::seq::SliceRandom;
use rand::thread_rng;
use serde::Deserialize;
use serde::Serialize;
use util::tunable;
use util::tunable::Percent;
use util::From64;

use crate::base_types::Extent;

tunable! {
    // 32MB is the biggest slab size that we can currently do without overflowing the u16 that
    // holds the number of slots in a BitRange structure with the default config of a 512B
    // bucket.
    pub static ref DEFAULT_SLAB_SIZE: ByteSize = ByteSize::mib(32);

    // The old index is freed as we write the new index, so we only need enough free slabs to
    // hold any increase in index size.
    pub static ref RESERVED_SLABS_PCT: Percent = Percent::new(2.0);

    // We never allow these slabs to be allocated.  If we get down to this few free slabs, we'll
    // panic.  By doing this before we get to zero, we preserve the possibility of changing
    // code/tunables to recover from running nearly out of space.
    static ref SUPER_RESERVED_SLABS_PCT: Percent = Percent::new(0.2);

    // Try to keep this amount of slabs available for normal (i.e. BlockAllocator) use.  Ideally
    // this will be enough space to absorb all the writes that can occur between index merges.
    static ref TARGET_AVAILABLE_SLABS_PCT: Percent = Percent::new(2.0);
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlabAllocatorPhys {
    slab_size: u64,
    capacity: Vec<Extent>,
}

#[derive(Debug)]
pub struct SlabAccess {
    capacity: BiBTreeMap<SlabId, Extent>,
    slab_size: u64,
    num_slabs: u64,
}

#[derive(Debug)]
pub struct SlabAllocatorBuilder {
    access: SlabAccess,
    allocatable: HashSet<SlabId>,
}

#[derive(Debug)]
pub struct SlabAllocator {
    access: SlabAccess,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    allocatable: Vec<SlabId>,
    freeing: Vec<SlabId>,
    reserved_slabs: u64,
}

impl SlabAllocatorPhys {
    pub fn new<I: IntoIterator<Item = Extent>>(capacity: I) -> Self {
        let mut this = Self {
            slab_size: DEFAULT_SLAB_SIZE.as_u64(),
            capacity: vec![],
        };
        this.extend(capacity);
        this
    }

    /// Add new capacity
    pub fn extend<I>(&mut self, capacity: I)
    where
        I: IntoIterator<Item = Extent>,
    {
        for extent in capacity {
            // capacity is aligned to be a multiple of slabsize
            self.capacity
                .push(extent.range(0, extent.size - extent.size % self.slab_size));
        }
    }

    pub fn capacity(&self) -> &[Extent] {
        &self.capacity
    }

    #[allow(dead_code)]
    pub fn slab_size(&self) -> u64 {
        self.slab_size
    }
}

impl SlabAccess {
    pub fn slab_id_to_extent(&self, slab_id: SlabId) -> Extent {
        let (&extent_slab, containing_extent) = self
            .capacity
            .left_range((Unbounded, Included(slab_id)))
            .next_back()
            .unwrap();
        containing_extent.range((slab_id - extent_slab) * self.slab_size, self.slab_size)
    }

    pub fn extent_to_slab_id(&self, extent: Extent) -> SlabId {
        assert_le!(extent.size, self.slab_size);

        let (&capacity_slab, capacity_extent) = self
            .capacity
            .right_range((Unbounded, Included(extent.location)))
            .next_back()
            .unwrap();

        assert!(capacity_extent.contains(&extent));
        let slab_id =
            capacity_slab + ((extent.location - capacity_extent.location) / self.slab_size);

        assert_lt!(slab_id.0, self.num_slabs);
        debug_assert!(self.slab_id_to_extent(slab_id).contains(&extent));
        slab_id
    }

    pub fn slab_size(&self) -> u64 {
        self.slab_size
    }

    pub fn num_slabs(&self) -> u64 {
        self.num_slabs
    }

    pub fn capacity(&self) -> u64 {
        self.num_slabs * self.slab_size
    }
}

impl SlabAllocatorBuilder {
    pub fn new(phys: SlabAllocatorPhys) -> Self {
        let mut num_slabs = 0;
        let capacity = phys
            .capacity
            .into_iter()
            .map(|extent| {
                let start = SlabId(num_slabs);
                num_slabs += extent.size / phys.slab_size;
                (start, extent)
            })
            .collect();
        Self {
            allocatable: (0..num_slabs).map(SlabId).collect(),
            access: SlabAccess {
                capacity,
                slab_size: phys.slab_size,
                num_slabs,
            },
        }
    }

    pub fn claim(&mut self, slab_id: SlabId) {
        let removed = self.allocatable.remove(&slab_id);
        assert!(removed);
    }

    pub fn build(self) -> SlabAllocator {
        SlabAllocator {
            inner: Mutex::new(Inner {
                allocatable: self.allocatable.into_iter().collect(),
                freeing: Vec::new(),
                reserved_slabs: RESERVED_SLABS_PCT.apply(self.access.num_slabs),
            }),
            access: self.access,
        }
    }

    pub fn access(&self) -> &SlabAccess {
        &self.access
    }

    #[allow(dead_code)]
    pub fn slab_id_to_extent(&self, slab_id: SlabId) -> Extent {
        self.access.slab_id_to_extent(slab_id)
    }

    pub fn extent_to_slab_id(&self, extent: Extent) -> SlabId {
        self.access.extent_to_slab_id(extent)
    }

    pub fn slab_size(&self) -> u64 {
        self.access.slab_size()
    }

    #[allow(dead_code)]
    pub fn num_slabs(&self) -> u64 {
        self.access.num_slabs()
    }

    #[allow(dead_code)]
    pub fn capacity(&self) -> u64 {
        self.access.capacity()
    }
}

impl SlabAllocator {
    pub fn get_phys(&self) -> SlabAllocatorPhys {
        SlabAllocatorPhys {
            slab_size: self.access.slab_size,
            capacity: self
                .access
                .capacity
                .iter()
                .map(|(_, &extent)| extent)
                .collect(),
        }
    }

    pub fn allocate(&self) -> Option<SlabId> {
        let mut inner = self.inner.lock().unwrap();

        if inner.allocatable.len() as u64 > inner.reserved_slabs {
            inner.allocatable.pop()
        } else {
            None
        }
    }

    /// Set the amount of reserved space (in bytes).  This space is for use by metadata (i.e.
    /// BlockBasedLog's), via allocate_reserved().  Note that the reserved space is not "used up"
    /// by allocate_reserved(), rather we try to always have this amount of available space,
    /// regardless of how much metadata is actually used.
    pub fn set_reservation(&self, reserved_space: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.reserved_slabs = max(
            reserved_space / self.access.slab_size,
            RESERVED_SLABS_PCT.apply(self.access.num_slabs),
        );
    }

    pub fn allocate_reserved(&self) -> SlabId {
        let mut inner = self.inner.lock().unwrap();
        if inner.allocatable.len() as u64 <= SUPER_RESERVED_SLABS_PCT.apply(self.access.num_slabs) {
            panic!("Free slabs exhausted.");
        }
        inner.allocatable.pop().unwrap()
    }

    pub fn free(&self, slab: SlabId) {
        self.inner.lock().unwrap().freeing.push(slab);
    }

    /// Returns the amount of non-reserved available space, in bytes. i.e. the amount that could
    /// be allocated by allocate().
    pub fn allocatable_bytes(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        (inner.allocatable.len() as u64).saturating_sub(inner.reserved_slabs)
            * self.access.slab_size
    }

    /// Returns the number of slabs that are not currently allocated.  This includes reserved and
    /// super-reserved slabs.
    pub fn free_slabs(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        inner.allocatable.len() as u64
    }

    /// Returns the number of slabs that we would like the block allocator to evacuate and free.
    pub fn num_slabs_to_evacuate(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        let target_free_slabs =
            inner.reserved_slabs + TARGET_AVAILABLE_SLABS_PCT.apply(self.access.num_slabs);
        let current_free_slabs = inner.allocatable.len() as u64;
        target_free_slabs.saturating_sub(current_free_slabs)
    }

    /// Release the space held by freed slabs, allowing them to be re-allocated.  This is safe to
    /// call after the checkpoint has been persisted to disk.
    pub fn release_frees(&self) {
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        // By using the `&mut Inner` directly, the borrow checker can understand the
        // `inner.allocatable` and `inner.freeing` below as "split borrows", allowing two
        // exclusive references to different fields of the same struct.
        inner.allocatable.append(&mut inner.freeing);
        inner.allocatable.shuffle(&mut thread_rng());
    }

    pub fn access(&self) -> &SlabAccess {
        &self.access
    }

    pub fn slab_id_to_extent(&self, slab_id: SlabId) -> Extent {
        self.access.slab_id_to_extent(slab_id)
    }

    pub fn extent_to_slab_id(&self, extent: Extent) -> SlabId {
        self.access.extent_to_slab_id(extent)
    }

    pub fn slab_size(&self) -> u64 {
        self.access.slab_size()
    }

    pub fn num_slabs(&self) -> u64 {
        self.access.num_slabs()
    }

    pub fn capacity(&self) -> u64 {
        self.access.capacity()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlabId(pub u64);
impl SlabId {
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

impl From<SlabId> for usize {
    fn from(val: SlabId) -> Self {
        usize::from64(val.0)
    }
}
