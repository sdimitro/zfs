use crate::base_types::*;
use crate::block_access::*;
use crate::features::FeatureName;
use anyhow::anyhow;
use anyhow::Result;
use futures::stream::*;
use log::*;
use serde::{Deserialize, Serialize};
use util::maybe_die_with;

pub const SUPERBLOCK_SIZE: u64 = 4 * 1024;

/// State stored at the beginning of every disk
#[derive(Serialize, Deserialize, Debug, Clone)]
struct SuperblockPhys {
    primary: Option<PrimaryPhys>,
    disk: DiskId,
    guid: u64,
    // XXX put sector size in here too and verify it matches what the disk says now?
    // XXX put disk size in here so we can detect expansion?
}

/// State that's only needed on the primary disk (currently, always DiskId(0)).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrimaryPhys {
    pub checkpoint_id: CheckpointId,
    pub checkpoint_capacity: Extent, // space available for checkpoints
    pub checkpoint: Extent,          // space used by latest checkpoint
    pub num_disks: usize,
    pub feature_flags: Vec<FeatureName>,
}

impl PrimaryPhys {
    /// Write superblocks to all disks.
    pub async fn write_all(&self, primary_disk: DiskId, guid: u64, block_access: &BlockAccess) {
        block_access
            .disks()
            .map(|disk| async move {
                // Change the DiskId of this superblock to match the disk we're writing to.
                let phys = if disk == primary_disk {
                    SuperblockPhys {
                        primary: Some(self.clone()),
                        disk,
                        guid,
                    }
                } else {
                    SuperblockPhys {
                        primary: None,
                        disk,
                        guid,
                    }
                };
                phys.write(block_access, disk).await;
            })
            .collect::<FuturesUnordered<_>>()
            .for_each(|_| async move {})
            .await;
    }

    /// Return value is (Self, primary_disk, guid, extra_disks)
    pub async fn read(block_access: &BlockAccess) -> Result<(Self, DiskId, u64, Vec<DiskId>)> {
        let results = SuperblockPhys::read_all(block_access).await;

        let (primary, primary_disk, guid) = results
            .iter()
            .find_map(|result| {
                if let Ok(phys) = result {
                    phys.primary
                        .as_ref()
                        .map(|primary| (primary.clone(), phys.disk, phys.guid))
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("Primary Superblock not found"))?;

        let extra_disks = results
            .iter()
            .enumerate()
            .filter_map(|(id, result)| match result {
                Ok(_) => None,
                Err(_) => Some(DiskId(id.try_into().unwrap())),
            })
            .collect::<Vec<_>>();

        for (id, result) in results.iter().enumerate() {
            // XXX proper error handling
            // XXX we should be able to reorder them?
            if let Ok(phys) = result {
                assert_eq!(DiskId(id.try_into().unwrap()), phys.disk);
                assert_eq!(phys.guid, guid);
                assert!(phys.primary.is_none() || phys.disk == primary_disk);
            }
        }

        assert_eq!(
            results.len() - extra_disks.len(),
            primary.num_disks,
            "Expected {} disks with superblocks, {} disks provided, of which {} have superblocks",
            primary.num_disks,
            results.len(),
            results.len() - extra_disks.len()
        );

        Ok((primary, primary_disk, guid, extra_disks))
    }
}

impl SuperblockPhys {
    async fn read(block_access: &BlockAccess, disk: DiskId) -> Result<SuperblockPhys> {
        let raw = block_access
            .read_raw(Extent::new(disk, 0, SUPERBLOCK_SIZE))
            .await;
        let (this, _): (Self, usize) = block_access.chunk_from_raw(&raw)?;
        debug!("got {:#?}", this);
        assert_eq!(this.disk, disk);
        Ok(this)
    }

    async fn read_all(block_access: &BlockAccess) -> Vec<Result<SuperblockPhys>> {
        block_access
            .disks()
            .map(|disk| SuperblockPhys::read(block_access, disk))
            .collect::<FuturesOrdered<_>>()
            .collect()
            .await
    }

    async fn write(&self, block_access: &BlockAccess, disk: DiskId) {
        maybe_die_with(|| format!("before writing {:#?}", self));
        debug!("writing {:#?}", self);
        let raw = block_access.chunk_to_raw(EncodeType::Json, self);
        // XXX pad it out to SUPERBLOCK_SIZE?
        block_access
            .write_raw(DiskLocation { offset: 0, disk }, raw)
            .await;
    }
}
