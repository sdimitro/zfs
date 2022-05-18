use std::collections::BTreeMap;

use anyhow::anyhow;
use anyhow::Result;
use futures::stream::*;
use log::*;
use serde::Deserialize;
use serde::Serialize;
use util::maybe_die_with;
use util::nice_p2size;
use util::writeln_stderr;
use util::writeln_stdout;
use util::zettacache_stats::DiskIoType;

use crate::base_types::*;
use crate::block_access::*;
use crate::checkpoint::CheckpointId;
use crate::features::FeatureName;
use crate::features::SUPPORTED_FEATURES;

pub const SUPERBLOCK_SIZE: u64 = util::message::SUPERBLOCK_SIZE as u64;

/// State stored at the beginning of every disk
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SuperblockPhys {
    pub primary: Option<PrimaryPhys>,
    pub disk: DiskId,
    pub guid: u64,
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

impl DiskPhys {
    pub fn new(size: u64) -> Self {
        Self { size }
    }
}

/// State that's only needed on the primary disk (currently, always DiskId(0)).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrimaryPhys {
    pub checkpoint_id: CheckpointId,
    pub feature_flags: Vec<FeatureName>,
    pub disks: BTreeMap<DiskId, DiskPhys>,

    // Each extent is a single slab, but the last extent can be a fraction of a
    // slab.  The remainder of that slab is uninitialized padding.
    pub checkpoint: Vec<Extent>,
}

/// Subset of PrimaryPhys that's needed to get the feature flags.
#[derive(Deserialize, Debug, Clone)]
pub struct PrimaryFeaturesPhys {
    feature_flags: Vec<FeatureName>,
}

impl PrimaryPhys {
    pub fn new(disks: BTreeMap<DiskId, DiskPhys>, checkpoint_extents: Vec<Extent>) -> Self {
        PrimaryPhys {
            checkpoint_id: CheckpointId(0),
            feature_flags: SUPPORTED_FEATURES.keys().cloned().collect(),
            disks,
            checkpoint: checkpoint_extents,
        }
    }

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
            .count()
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
                Err(_) => Some(DiskId::new(id)),
            })
            .collect::<Vec<_>>();

        for (id, result) in results.iter().enumerate() {
            // XXX proper error handling
            // XXX we should be able to reorder them?
            if let Ok(phys) = result {
                assert_eq!(DiskId::new(id), phys.disk);
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
    async fn read_impl(block_access: &BlockAccess, disk: DiskId) -> Result<Self> {
        let raw = block_access
            .read_raw(
                Extent::new(disk, 0, SUPERBLOCK_SIZE),
                DiskIoType::MaintenanceRead,
            )
            .await;
        let (this, _): (Self, usize) = block_access.chunk_from_raw(&raw)?;
        Ok(this)
    }

    async fn read(block_access: &BlockAccess, disk: DiskId) -> Result<Self> {
        let this = Self::read_impl(block_access, disk).await?;
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
                DiskLocation::new(disk, 0),
                raw,
                DiskIoType::MaintenanceWrite,
            )
            .await;
    }

    pub async fn dump_all(block_access: &BlockAccess) {
        // For this to work correctly when we don't specify all the cache disks
        // in zcachedb in order, we differentiate between the DiskId used in
        // BlockAccess (e.g. zcachedb's disk argument order) and what we read
        // from the actual disk's state (e.g. its Superblock).
        for block_access_disk_id in block_access.disks() {
            match Self::read_impl(block_access, block_access_disk_id).await {
                Ok(superblock) => writeln_stdout!(
                    "{:?} - Path: {:?} Size: {} GUID: {} Primary?: {}",
                    superblock.disk,
                    block_access.disk_path(block_access_disk_id),
                    nice_p2size(block_access.disk_size(block_access_disk_id)),
                    superblock.guid,
                    match &superblock.primary {
                        Some(primary) => format!("{:#?}", primary),
                        None => "No".to_string(),
                    }
                ),
                Err(_) => writeln_stderr!(
                    "error: {:?}: not a valid zettacache disk",
                    block_access.disk_path(block_access_disk_id)
                ),
            }
        }
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
                assert_eq!(DiskId::new(id), phys.disk);
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
