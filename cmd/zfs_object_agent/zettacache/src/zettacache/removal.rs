use chrono::DateTime;
use chrono::Local;
use log::info;
use serde::Deserialize;
use serde::Serialize;
use util::RemovalStatus;

use super::TARGET_FREE_BLOCKS_PCT;
use crate::base_types::DiskId;
use crate::block_allocator::BlockAllocator;
use crate::slab_allocator::SlabAllocator;

#[derive(Default, Serialize, Deserialize, Debug, Clone)]
pub struct DeviceRemovalPhys {
    pub pending: Vec<DiskId>,
    paused: bool,
}

pub struct DeviceRemovalEntry {
    pub disk: DiskId,

    // Indicates the number of merge cycles that we expect for the removal of all index-related
    // metadata from the removing device. Value is None if we are still waiting for the removal
    // of BlockAllocator's data and miscellaneous metadata not related to the index (see comment
    // in Locked::flush_checkpoint).
    pub merge_cycles_left: Option<u8>,

    pub total_space_to_evacuate: u64,

    // The time that we started evacuating user data away from that device. Set to None before
    // that point.
    pub start_time: Option<DateTime<Local>>,
}

pub struct DeviceRemoval {
    pending: Vec<DeviceRemovalEntry>,
    pub paused: bool,
}

impl DeviceRemoval {
    pub fn open(phys: DeviceRemovalPhys, block_allocator: &BlockAllocator) -> Self {
        DeviceRemoval {
            pending: phys
                .pending
                .iter()
                .map(|&disk| DeviceRemovalEntry {
                    disk,
                    merge_cycles_left: None,
                    total_space_to_evacuate: block_allocator.disk_space_to_evacuate(disk),
                    start_time: None,
                })
                .collect(),
            paused: phys.paused,
        }
    }

    pub fn to_phys(&self) -> DeviceRemovalPhys {
        DeviceRemovalPhys {
            pending: self
                .pending
                .iter()
                .map(|removal_entry| removal_entry.disk)
                .collect(),
            paused: self.paused,
        }
    }

    pub fn get_status(
        &self,
        disk: DiskId,
        block_allocator: &BlockAllocator,
        slab_allocator: &SlabAllocator,
    ) -> Option<RemovalStatus> {
        if let Some(idx) = self
            .pending
            .iter()
            .position(|removal_entry| removal_entry.disk == disk)
        {
            let removal_entry = &self.pending[idx];
            let currently_removing_device = idx == 0;
            let space_left_to_evacuate = block_allocator.disk_space_to_evacuate(disk);
            let mut space_left_to_evict = 0;

            if currently_removing_device
                && space_left_to_evacuate != 0
                && removal_entry.start_time.is_none()
            {
                let allocatable_space =
                    slab_allocator.allocatable_bytes() + block_allocator.allocatable_bytes();
                let slab_allocator_capacity =
                    slab_allocator.capacity() - slab_allocator.removing_capacity();
                let slop = TARGET_FREE_BLOCKS_PCT.apply(slab_allocator_capacity);

                space_left_to_evict = (removal_entry.total_space_to_evacuate + slop)
                    .saturating_sub(allocatable_space);
            }
            return Some(RemovalStatus {
                currently_removing_device,
                total_space_to_evacuate: removal_entry.total_space_to_evacuate,
                space_left_to_evacuate,
                space_left_to_evict,
                start_time: removal_entry.start_time,
            });
        }
        None
    }

    /// Return the DiskId that we are actively removing.
    pub fn removing_disk(&self) -> Option<DiskId> {
        self.pending.get(0).map(|removal_entry| removal_entry.disk)
    }

    /// Return the DeviceRemovalEntry of the disk we are actively removing.
    pub fn removing_disk_entry(&mut self) -> Option<&mut DeviceRemovalEntry> {
        self.pending.get_mut(0)
    }

    pub fn disks_in_queue(&self) -> usize {
        self.pending.len()
    }

    pub fn add_to_queue(&mut self, disk: DiskId, total_space_to_evacuate: u64) {
        self.pending.push(DeviceRemovalEntry {
            disk,
            merge_cycles_left: None,
            total_space_to_evacuate,
            start_time: None,
        });
    }

    pub fn remove_from_queue(&mut self, disk: DiskId) {
        let pos = self
            .pending
            .iter()
            .position(|removal_entry| removal_entry.disk == disk)
            .unwrap();
        self.pending.remove(pos);
    }

    pub fn disk_is_pending_removal(&self, disk: DiskId) -> bool {
        self.pending
            .iter()
            .any(|removal_entry| removal_entry.disk == disk)
    }

    /// Returns true if we are in the last stage of removal where we are waiting for
    /// the index metadata to be evacuated from our currently removing disk. False
    /// otherwise.
    pub fn need_index_evacuation(&self) -> bool {
        if let Some(removal_entry) = self.pending.get(0) {
            return removal_entry.merge_cycles_left.is_some();
        }
        false
    }

    /// Called at the completion of each merge cycle, we decrement the `merge_cycles_left`
    /// counter of the removal in progress as a sign for tracking our progress (see comment in
    /// Locked::flush_checkpoint).
    pub fn complete_merge_cycle(&mut self) {
        if let Some(removal_entry) = self.pending.get_mut(0) {
            if let Some(cycles_left) = removal_entry.merge_cycles_left.as_mut() {
                *cycles_left = cycles_left.checked_sub(1).unwrap();
                info!(
                    "removal: {cycles_left} merge cycles left for {:?}",
                    removal_entry.disk
                )
            }
        }
    }

    pub fn complete_removal(&mut self, disk: DiskId) {
        let removal_entry = self.pending.remove(0);
        assert_eq!(removal_entry.disk, disk)
    }
}
