//! The disk is divided into equal-size slabs (by default, 32MB each).  The SlabAllocator knows
//! which slabs are allocated and which are free.  Other subsystems (e.g. BlockBasedLogs, the
//! BlockAllocator, and Checkpoints) can call into the SlabAllocator to allocate slabs for their
//! own use.
//!
//! By contrast, the SlabAllocatorPhys does not include information about which slabs are
//! allocated and which are freed.  When opening the Zettacache, before any slab allocations can
//! be performed, the other subsystems (e.g.  BBL, BlockAllocator, Checkpoint) must tell the Slab
//! Allocator which slabs are allocated, by calling SlabAllocatorBuilder::claim().

use std::cmp::max;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::mem;
use std::ops::Add;
use std::ops::AddAssign;
use std::ops::Bound::*;
use std::ops::Sub;
use std::sync::Mutex;
use std::sync::RwLock;

use bimap::BiBTreeMap;
use bytesize::ByteSize;
use log::*;
use more_asserts::*;
use rand::seq::SliceRandom;
use rand::thread_rng;
use serde::Deserialize;
use serde::Serialize;
use util::measure;
use util::tunable;
use util::tunable::Percent;
use util::From64;
use util::VecMap;

use crate::base_types::DiskId;
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

// Note: Before device removal we used `Extent`s to indicate a present chunk of
// capacity.  The `untagged` and `flatten` serde annotations below are used to
// make new bits backwards compatible with the old on-disk format.
#[derive(Debug, Serialize, Deserialize, Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
#[serde(untagged)]
enum SlabExtentPhys {
    Present {
        #[serde(flatten)]
        extent: Extent,
    },
    Removed {
        extent: Extent,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlabAllocatorPhys {
    slab_size: u64,
    capacity: Vec<SlabExtentPhys>,
}

#[derive(Debug)]
pub struct SlabAccess {
    inner: RwLock<SlabAccessInner>,
    slab_size: u64,
}

#[derive(Debug)]
struct SlabAccessInner {
    capacity: BTreeMap<SlabId, SlabExtentPhys>,
    active_capacity: BiBTreeMap<SlabId, Extent>,
    num_slabs: u64,
    next_slab_id: SlabId,
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

    // If there is an entry in the map below, then the entire disk has been marked as noalloc,
    // and none of its slabs will appear in `allocatable`. When the disk is finally removed, its
    // key is removed from this map. Note that the slab IDs that belong to a removed disk are
    // never re-used for the lifetime of the cache.
    noalloc_state: VecMap<DiskId, Vec<SlabId>>,
    // Slabs that are released/freed back to the SlabAllocator and belong to a removing disk, end
    // up here instead of `freeing`, and eventually make it back to the noalloc_state for that
    // disk.
    noallocing: Vec<SlabId>,

    reserved_slabs: u64,
}

impl Inner {
    fn num_removing_slabs(&self, access: &SlabAccess) -> u64 {
        self.noalloc_state
            .keys()
            .map(|disk| access.disk_num_slabs(disk))
            .sum::<u64>()
    }
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
            self.capacity.push(SlabExtentPhys::Present {
                extent: extent.range(0, extent.size - extent.size % self.slab_size),
            });
        }
    }

    pub fn active_capacity_bytes(&self) -> u64 {
        self.capacity
            .iter()
            .map(|extent_phys| match extent_phys {
                SlabExtentPhys::Present { extent } => extent.size,
                SlabExtentPhys::Removed { extent: _ } => 0,
            })
            .sum()
    }

    #[allow(dead_code)]
    pub fn slab_size(&self) -> u64 {
        self.slab_size
    }
}

impl SlabAccess {
    pub fn slab_id_to_extent(&self, slab_id: SlabId) -> Extent {
        let inner = self.inner.read().unwrap();
        let (&extent_slab, containing_extent) = inner
            .active_capacity
            .left_range((Unbounded, Included(slab_id)))
            .next_back()
            .unwrap();
        containing_extent.range((slab_id - extent_slab) * self.slab_size, self.slab_size)
    }

    pub fn extent_to_slab_id(&self, extent: Extent) -> SlabId {
        assert_le!(extent.size, self.slab_size);

        let inner = self.inner.read().unwrap();
        let (&capacity_slab, capacity_extent) = inner
            .active_capacity
            .right_range((Unbounded, Included(extent.location)))
            .next_back()
            .unwrap();

        assert!(capacity_extent.contains(&extent));
        let slab_id =
            capacity_slab + ((extent.location - capacity_extent.location) / self.slab_size);

        debug_assert!(self.slab_id_to_extent(slab_id).contains(&extent));
        slab_id
    }

    pub fn slab_size(&self) -> u64 {
        self.slab_size
    }

    pub fn num_slabs(&self) -> u64 {
        let inner = self.inner.read().unwrap();
        inner.num_slabs
    }

    pub fn capacity(&self) -> u64 {
        let inner = self.inner.read().unwrap();
        inner.num_slabs * self.slab_size
    }

    pub fn disk_capacity(&self, disk: DiskId) -> u64 {
        let inner = self.inner.read().unwrap();
        inner
            .active_capacity
            .iter()
            .filter(|(_, &extent)| extent.location.disk() == disk)
            .map(|(_, &extent)| extent.size)
            .sum()
    }

    pub fn disk_num_slabs(&self, disk: DiskId) -> u64 {
        self.disk_capacity(disk) / self.slab_size()
    }
}

impl SlabAllocatorBuilder {
    pub fn new(phys: SlabAllocatorPhys) -> Self {
        let mut allocatable = HashSet::new();
        let mut active_capacity = BiBTreeMap::new();

        let mut total_slabs = 0;
        let mut removed_slabs = 0;
        let capacity = phys
            .capacity
            .into_iter()
            .map(|extent_phys| {
                let start = SlabId(total_slabs);
                match extent_phys {
                    SlabExtentPhys::Present { extent } => {
                        active_capacity.insert(start, extent);
                        let nslabs = extent.size / phys.slab_size;
                        allocatable.extend(
                            (total_slabs..(total_slabs + nslabs))
                                .map(SlabId)
                                .collect::<HashSet<SlabId>>(),
                        );
                        total_slabs += nslabs;
                    }
                    SlabExtentPhys::Removed { extent } => {
                        let nslabs = extent.size / phys.slab_size;
                        removed_slabs += nslabs;
                        total_slabs += nslabs;
                    }
                }
                (start, extent_phys)
            })
            .collect();

        Self {
            allocatable,
            access: SlabAccess {
                inner: RwLock::new(SlabAccessInner {
                    capacity,
                    active_capacity,
                    num_slabs: total_slabs - removed_slabs,
                    next_slab_id: SlabId(total_slabs),
                }),
                slab_size: phys.slab_size,
            },
        }
    }

    pub fn claim(&mut self, slab_id: SlabId) {
        let removed = self.allocatable.remove(&slab_id);
        assert!(removed, "{slab_id:?} claimed twice");
    }

    pub fn build(self, removing_disks: &[DiskId]) -> SlabAllocator {
        let mut noalloc_slabs = removing_disks
            .iter()
            .copied()
            .map(|disk| (disk, Vec::new()))
            .collect::<VecMap<_, _>>();
        let mut allocatable = Vec::new();
        for slab_id in self.allocatable.into_iter() {
            let slab_disk = self.access.slab_id_to_extent(slab_id).location.disk();

            noalloc_slabs
                .get_mut(slab_disk)
                .unwrap_or(&mut allocatable)
                .push(slab_id);
        }

        SlabAllocator {
            inner: Mutex::new(Inner {
                allocatable,
                freeing: Vec::new(),
                noalloc_state: noalloc_slabs,
                noallocing: Vec::new(),
                reserved_slabs: RESERVED_SLABS_PCT.apply(self.access.num_slabs()),
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

    #[allow(dead_code)]
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
    pub fn extend(&self, capacity: Extent) {
        // lock order: SlabAllocator.inner before SlabAccess.inner
        let mut inner = self.inner.lock().unwrap();

        let mut access_inner = self.access.inner.write().unwrap();
        let first_new_slab = access_inner.next_slab_id;
        // capacity is aligned to be a multiple of slabsize
        let capacity = capacity.trim_end(capacity.size - capacity.size % self.access.slab_size);
        access_inner
            .capacity
            .insert(first_new_slab, SlabExtentPhys::Present { extent: capacity });
        access_inner
            .active_capacity
            .insert(first_new_slab, capacity);
        let new_slabs = capacity.size / self.access.slab_size;

        // We don't want to allocate and write to the new capacity until the next checkpoint
        // (when the SuperBlockPhys's have been updated to reflect the new capacity).  Therefore
        // we add the new slabs to `freeing`.
        inner
            .freeing
            .extend((0..new_slabs).map(|i| access_inner.next_slab_id + i));

        access_inner.num_slabs += new_slabs;
        access_inner.next_slab_id += new_slabs;
    }

    pub fn remove_disk(&self, disk: DiskId) {
        assert!(self.disk_is_fully_evacuated(disk));

        let mut inner = self.inner.lock().unwrap();
        let mut access_inner_guard = self.access.inner.write().unwrap();
        let access_inner = &mut *access_inner_guard;
        let removed_slabs = inner.noalloc_state.remove(disk).unwrap();

        access_inner.active_capacity.retain(|&slab_id, &extent| {
            if extent.location.disk() == disk {
                let old = access_inner
                    .capacity
                    .insert(slab_id, SlabExtentPhys::Removed { extent });
                assert_eq!(old, Some(SlabExtentPhys::Present { extent }));
                trace!(
                    "removal: removing {:?} from slab allocator capacity",
                    old.unwrap()
                );
                false
            } else {
                true
            }
        });
        access_inner.num_slabs -= removed_slabs.len() as u64;

        // Dropping guard as writer so the assertions below can grab guard as reader in
        // slab_id_to_extent().
        drop(access_inner_guard);
        assert!(!inner.allocatable.iter().any(|&slab_id| self
            .slab_id_to_extent(slab_id)
            .location
            .disk()
            == disk));
        assert!(!inner.freeing.iter().any(|&slab_id| self
            .slab_id_to_extent(slab_id)
            .location
            .disk()
            == disk));
        assert!(!inner.noallocing.iter().any(|&slab_id| self
            .slab_id_to_extent(slab_id)
            .location
            .disk()
            == disk));
    }

    /// Marks all slabs that are part of `disk` as non-allocatable. No further allocations can be
    /// made from these slabs and they won't be reused once freed.
    pub fn mark_noalloc_disk(&self, disk: DiskId) {
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        let mut removing_allocatable = Vec::new();
        inner.allocatable.retain(|&slab_id| {
            if self.slab_id_to_extent(slab_id).location.disk() == disk {
                removing_allocatable.push(slab_id);
                false
            } else {
                true
            }
        });
        inner.freeing.retain(|&slab_id| {
            if self.slab_id_to_extent(slab_id).location.disk() == disk {
                inner.noallocing.push(slab_id);
                false
            } else {
                true
            }
        });
        let replaced = inner.noalloc_state.insert(disk, removing_allocatable);
        assert!(replaced.is_none());
    }

    /// Marks all free slabs that are part of `disk` as allocatable. This also enables
    /// the reuse of allocated slabs that are freed.
    pub fn unmark_noalloc_disk(&self, disk: DiskId) {
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        inner
            .allocatable
            .append(&mut inner.noalloc_state.remove(disk).unwrap());
        inner.freeing.append(&mut inner.noallocing);
        inner.allocatable.shuffle(&mut thread_rng());
    }

    pub fn get_phys(&self) -> SlabAllocatorPhys {
        let access_inner = self.access.inner.read().unwrap();
        SlabAllocatorPhys {
            slab_size: self.access.slab_size,
            capacity: access_inner
                .capacity
                .iter()
                .map(|(_, &extent)| extent)
                .collect(),
        }
    }

    pub fn allocate(&self) -> Option<SlabId> {
        let mut inner = self.inner.lock().unwrap();

        if inner.allocatable.len() as u64 > inner.reserved_slabs {
            let slab = inner.allocatable.pop();
            trace!("allocating {slab:?}");
            slab
        } else {
            measure!("slab allocation failed").hit();
            trace!("slab allocation failed");
            None
        }
    }

    /// Set the amount of reserved space (in bytes).  This space is for use by metadata (i.e.
    /// BlockBasedLog's), via allocate_reserved().  Note that the reserved space is not "used up"
    /// by allocate_reserved(), rather we try to always have this amount of available space,
    /// regardless of how much metadata is actually used.
    pub fn set_reservation(&self, reserved_space: u64) {
        let mut inner = self.inner.lock().unwrap();
        let removing_slabs = inner.num_removing_slabs(&self.access);
        inner.reserved_slabs = max(
            reserved_space / self.access.slab_size,
            RESERVED_SLABS_PCT.apply(self.access.num_slabs() - removing_slabs),
        );
    }

    pub fn allocate_reserved(&self) -> SlabId {
        let mut inner = self.inner.lock().unwrap();
        let removing_slabs = inner.num_removing_slabs(&self.access);
        if inner.allocatable.len() as u64
            <= SUPER_RESERVED_SLABS_PCT.apply(self.access.num_slabs() - removing_slabs)
        {
            panic!("Free slabs exhausted.");
        }
        let slab = inner.allocatable.pop().unwrap();
        trace!("allocating reserved {slab:?}");
        slab
    }

    pub fn free(&self, slab: SlabId) {
        trace!("freeing {slab:?}");
        let extent_disk = self.access().slab_id_to_extent(slab).location.disk();
        let mut inner = self.inner.lock().unwrap();
        if inner.noalloc_state.contains_key(&extent_disk) {
            inner.noallocing.push(slab);
        } else {
            inner.freeing.push(slab);
        }
    }

    /// Returns the amount of non-reserved available space, in bytes. i.e. the amount that could
    /// be allocated by allocate().
    ///
    /// Note: Slabs that are part of a disk that's been `mark_noalloc_disk()`-ed are not
    /// allocatable, and therefore not included in returned value.
    pub fn allocatable_bytes(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        (inner.allocatable.len() as u64).saturating_sub(inner.reserved_slabs)
            * self.access.slab_size
    }

    /// Returns the number of slabs that are not currently allocated.  This includes reserved and
    /// super-reserved slabs and the ones marked as noalloc for removal.
    pub fn free_slabs(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        let noalloc_slabs = inner
            .noalloc_state
            .values()
            .map(|slabs| slabs.len())
            .sum::<usize>() as u64;
        inner.allocatable.len() as u64 + noalloc_slabs
    }

    /// Returns the number of slabs that we would like the block allocator to evacuate and free.
    pub fn num_slabs_to_evacuate(&self) -> u64 {
        let inner = self.inner.lock().unwrap();
        let removing_slabs = inner.num_removing_slabs(&self.access);
        let target_free_slabs = inner.reserved_slabs
            + TARGET_AVAILABLE_SLABS_PCT.apply(self.access.num_slabs() - removing_slabs);
        let current_free_slabs = inner.allocatable.len() + inner.freeing.len();
        target_free_slabs.saturating_sub(current_free_slabs as u64)
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
        for slab_id in mem::take(&mut inner.noallocing) {
            let slab_disk = self.slab_id_to_extent(slab_id).location.disk();
            inner
                .noalloc_state
                .get_mut(slab_disk)
                .unwrap()
                .push(slab_id);
        }
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
        let removing_slabs = self.inner.lock().unwrap().num_removing_slabs(&self.access);
        self.access.num_slabs() - removing_slabs
    }

    pub fn capacity(&self) -> u64 {
        self.access.capacity()
    }

    pub fn disk_capacity(&self, disk: DiskId) -> u64 {
        self.access.disk_capacity(disk)
    }

    /// Returns the number of slabs that are currently marked as allocated/in-use from the slab
    /// allocators perspective. To be called only for devices that are being removed.
    pub fn disk_slabs_to_evacuate(&self, disk: DiskId) -> u64 {
        let disk_num_slabs = self.access.disk_num_slabs(disk);
        let noalloc_slabs = self
            .inner
            .lock()
            .unwrap()
            .noalloc_state
            .get(disk)
            .map(|slabs| slabs.len())
            .unwrap() as u64;
        disk_num_slabs.checked_sub(noalloc_slabs).unwrap()
    }

    pub fn disk_is_fully_evacuated(&self, disk: DiskId) -> bool {
        self.disk_slabs_to_evacuate(disk) == 0
    }

    pub fn removing_capacity(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .noalloc_state
            .keys()
            .map(|disk_id| self.disk_capacity(disk_id))
            .sum()
    }

    pub fn removing_disks(&self) -> impl Iterator<Item = DiskId> {
        self.inner
            .lock()
            .unwrap()
            .noalloc_state
            .keys()
            .collect::<Vec<_>>()
            .into_iter()
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

impl AddAssign<u64> for SlabId {
    fn add_assign(&mut self, other: u64) {
        *self = SlabId(self.0 + other);
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

impl From<usize> for SlabId {
    fn from(val: usize) -> Self {
        SlabId(val as u64)
    }
}
