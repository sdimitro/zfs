use std::collections::BTreeMap;

use crate::base_types::*;
use crate::block_access::*;
use crate::features::FeatureName;
use anyhow::anyhow;
use anyhow::Result;
use futures::stream::*;
use log::*;
use serde::{Deserialize, Serialize};
use util::maybe_die_with;
use util::zettacache_stats::DiskIoType;

pub const SUPERBLOCK_SIZE: u64 = 4 * 1024;

/// State stored at the beginning of every disk
#[derive(Serialize, Deserialize, Debug, Clone)]
struct SuperblockPhys {
    primary: Option<PrimaryPhys>,
    disk: DiskId,
    guid: u64,
}

/// Subset of SuperblockPhys that's needed to get the feature flags.
#[derive(Deserialize, Debug)]
struct SuperblockFeaturesPhys {
    primary: Option<PrimaryFeaturesPhys>,
    disk: DiskId,
    guid: u64,
}

/// State stored about every disk
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskPhys {
    pub size: u64,
    // XXX put sector size in here too and verify it matches what the disk says now?
}

/// State that's only needed on the primary disk (currently, always DiskId(0)).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrimaryPhys {
    pub checkpoint_id: CheckpointId,
    pub checkpoint_capacity: Extent, // space available for checkpoints
    pub checkpoint: Extent,          // space used by latest checkpoint
    pub old_checkpoint_capacity: Vec<Extent>, // unused space, previously used for checkpoints
    pub feature_flags: Vec<FeatureName>,
    pub disks: BTreeMap<DiskId, DiskPhys>,
}

/// Subset of PrimaryPhys that's needed to get the feature flags.
#[derive(Deserialize, Debug, Clone)]
pub struct PrimaryFeaturesPhys {
    feature_flags: Vec<FeatureName>,
}

impl PrimaryPhys {
    /// Write superblocks to all disks.
    pub async fn write_all(&self, primary_disk: DiskId, guid: u64, block_access: &BlockAccess) {
        // Write the non-primary disks first, so that newly-added disks will
        // have their superblocks present before the primary superblock is
        // updated to indicate that they are part of the cache.  If we wrote all
        // the disks (including the primary) at once, we could crash after the
        // primary was updated but a new disk had not yet been updated.  The
        // cache would be left in an inconsistent state and could not be opened.
        block_access
            .disks()
            .filter(|&disk| disk != primary_disk)
            .map(|disk| async move {
                let phys = SuperblockPhys {
                    primary: None,
                    disk,
                    guid,
                };
                phys.write(block_access, disk).await;
            })
            .collect::<FuturesUnordered<_>>()
            .for_each(|_| async move {})
            .await;

        let phys = SuperblockPhys {
            primary: Some(self.clone()),
            disk: primary_disk,
            guid,
        };
        phys.write(block_access, primary_disk).await;
    }

    pub async fn read_features(block_access: &BlockAccess) -> Result<Vec<FeatureName>> {
        PrimaryFeaturesPhys::read(block_access)
            .await
            .map(|phys| phys.feature_flags)
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
            primary.disks.len(),
            "Expected {} disks with superblocks, {} disks provided, of which {} have superblocks",
            primary.disks.len(),
            results.len(),
            results.len() - extra_disks.len()
        );

        Ok((primary, primary_disk, guid, extra_disks))
    }
}

impl SuperblockPhys {
    async fn read(block_access: &BlockAccess, disk: DiskId) -> Result<Self> {
        let raw = block_access
            .read_raw(
                Extent::new(disk, 0, SUPERBLOCK_SIZE),
                DiskIoType::MaintenanceRead,
            )
            .await;
        let (this, _): (Self, usize) = block_access.chunk_from_raw(&raw)?;
        debug!("got {:#?}", this);
        assert_eq!(this.disk, disk);
        Ok(this)
    }

    async fn read_all(block_access: &BlockAccess) -> Vec<Result<Self>> {
        block_access
            .disks()
            .map(|disk| Self::read(block_access, disk))
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
            .write_raw(
                DiskLocation { offset: 0, disk },
                raw,
                DiskIoType::MaintenanceWrite,
            )
            .await;
    }
}

impl PrimaryFeaturesPhys {
    /// Return value is (Self, primary_disk, guid, extra_disks)
    pub async fn read(block_access: &BlockAccess) -> Result<Self> {
        let results = SuperblockFeaturesPhys::read_all(block_access).await;

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

        for (id, result) in results.iter().enumerate() {
            // XXX proper error handling
            // XXX we should be able to reorder them?
            if let Ok(phys) = result {
                assert_eq!(DiskId(id.try_into().unwrap()), phys.disk);
                assert_eq!(phys.guid, guid);
                assert!(phys.primary.is_none() || phys.disk == primary_disk);
            }
        }

        Ok(primary)
    }
}

impl SuperblockFeaturesPhys {
    async fn read(block_access: &BlockAccess, disk: DiskId) -> Result<Self> {
        let raw = block_access
            .read_raw(
                Extent::new(disk, 0, SUPERBLOCK_SIZE),
                DiskIoType::MaintenanceRead,
            )
            .await;
        let (this, _): (Self, usize) = block_access.chunk_from_raw(&raw)?;
        debug!("got {:#?}", this);
        assert_eq!(this.disk, disk);
        Ok(this)
    }

    async fn read_all(block_access: &BlockAccess) -> Vec<Result<Self>> {
        block_access
            .disks()
            .map(|disk| Self::read(block_access, disk))
            .collect::<FuturesOrdered<_>>()
            .collect()
            .await
    }
}
