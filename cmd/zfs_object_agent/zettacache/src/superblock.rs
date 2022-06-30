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
    #[serde(rename = "guid")]
    pub cache_guid: CacheGuid,
    #[serde(default)]
    pub disk_guid: Option<DiskGuid>,
}

/// Subset of SuperblockPhys that's needed to get the feature flags.
#[derive(Deserialize, Debug)]
struct SuperblockFeaturesPhys {
    primary: Option<PrimaryFeaturesPhys>,
    disk: DiskId,
    #[serde(rename = "guid")]
    cache_guid: CacheGuid,
}

/// State stored about every disk
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiskPhys {
    pub size: u64,
    #[serde(default)]
    pub guid: DiskGuid,
}

impl DiskPhys {
    pub fn new(size: u64) -> Self {
        Self {
            size,
            guid: DiskGuid::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrimaryPhys {
    pub checkpoint_id: CheckpointId,
    pub feature_flags: Vec<FeatureName>,
    pub disks: BTreeMap<DiskId, DiskPhys>,
    #[serde(default)]
    pub sector_size: Option<u64>,

    // Each extent is a single slab, but the last extent can be a fraction of a
    // slab.  The remainder of that slab is uninitialized padding.
    pub checkpoint: Vec<Extent>,
}

/// Subset of PrimaryPhys that's needed to get the feature flags.
#[derive(Deserialize, Debug, Clone)]
pub struct PrimaryFeaturesPhys {
    // The checkpoint_id is needed so we can pick the latest primary
    checkpoint_id: CheckpointId,
    feature_flags: Vec<FeatureName>,
}

impl PrimaryPhys {
    pub fn new(
        disks: BTreeMap<DiskId, DiskPhys>,
        sector_size: u64,
        checkpoint_extents: Vec<Extent>,
    ) -> Self {
        PrimaryPhys {
            checkpoint_id: CheckpointId(0),
            feature_flags: SUPPORTED_FEATURES.keys().cloned().collect(),
            disks,
            sector_size: Some(sector_size),
            checkpoint: checkpoint_extents,
        }
    }

    pub async fn write(
        &self,
        primary_disk: DiskId,
        cache_guid: CacheGuid,
        block_access: &BlockAccess,
    ) {
        let phys = SuperblockPhys {
            primary: Some(self.clone()),
            disk: primary_disk,
            cache_guid,
            disk_guid: Some(self.disks.get(&primary_disk).unwrap().guid),
        };
        phys.write(block_access, primary_disk).await;
    }

    /// Write superblocks to all disks. Used during cache creation.
    pub async fn write_all(
        &self,
        primary_disk: DiskId,
        cache_guid: CacheGuid,
        block_access: &BlockAccess,
    ) {
        block_access
            .disks()
            .filter(|&disk| disk != primary_disk)
            .map(|disk| async move {
                let phys = SuperblockPhys {
                    primary: None,
                    disk,
                    cache_guid,
                    disk_guid: Some(self.disks.get(&disk).unwrap().guid),
                };
                phys.write(block_access, disk).await;
            })
            .collect::<FuturesUnordered<_>>()
            .count()
            .await;
        self.write(primary_disk, cache_guid, block_access).await;
    }

    pub async fn read_features(block_access: &BlockAccess) -> Result<Vec<FeatureName>> {
        PrimaryFeaturesPhys::read(block_access)
            .await
            .map(|phys| phys.feature_flags)
    }

    /// Return value is (Self, primary_disk, cache_guid, extra_disks)
    pub async fn read(
        block_access: &BlockAccess,
    ) -> Result<(Self, DiskId, CacheGuid, Vec<DiskId>)> {
        let results = SuperblockPhys::read_all(block_access).await;

        let (mut primary, primary_disk, cache_guid) = results
            .iter()
            .flatten()
            .max_by_key(|phys| phys.primary.as_ref().map(|p| p.checkpoint_id))
            .map(|phys| {
                (
                    phys.primary.as_ref().unwrap().clone(),
                    phys.disk,
                    phys.cache_guid,
                )
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

        // XXX proper error handling
        for phys in results.iter().flatten() {
            assert_eq!(phys.cache_guid, cache_guid);
            assert!(phys.disk != primary_disk || phys.primary.is_some());
            if let Some(disk_guid) = phys.disk_guid {
                assert_eq!(disk_guid, primary.disks.get(&phys.disk).unwrap().guid);
            }
        }
        let sector_size = block_access.round_up_to_sector::<u64>(1);
        if let Some(recorded_sector_size) = primary.sector_size {
            assert_eq!(recorded_sector_size, sector_size);
        } else {
            primary.sector_size = Some(sector_size);
        }

        assert_eq!(
            results.len() - extra_disks.len(),
            primary.disks.len(),
            "Expected {} disks with superblocks, {} disks provided, of which {} have superblocks",
            primary.disks.len(),
            results.len(),
            results.len() - extra_disks.len()
        );

        Ok((primary, primary_disk, cache_guid, extra_disks))
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

    pub async fn write(&self, block_access: &BlockAccess, disk: DiskId) {
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
        maybe_die_with(|| format!("after writing {:#?}", self));
    }

    pub async fn dump_all(block_access: &BlockAccess) {
        // For this to work correctly when we don't specify all the cache disks
        // in zcachedb in order, we differentiate between the DiskId used in
        // BlockAccess (e.g. zcachedb's disk argument order) and what we read
        // from the actual disk's state (e.g. its Superblock).
        for block_access_disk_id in block_access.disks() {
            match Self::read_impl(block_access, block_access_disk_id).await {
                Ok(superblock) => writeln_stdout!(
                    "{:?} - Path: {:?} Size: {} {:?} {:?} Primary?: {}",
                    superblock.disk,
                    block_access.disk_path(block_access_disk_id),
                    nice_p2size(block_access.disk_size(block_access_disk_id)),
                    superblock.disk_guid,
                    superblock.cache_guid,
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
    pub async fn read(block_access: &BlockAccess) -> Result<Self> {
        let results = SuperblockFeaturesPhys::read_all(block_access).await;

        let (primary, primary_disk, cache_guid) = results
            .iter()
            .flatten()
            .max_by_key(|phys| phys.primary.as_ref().map(|p| p.checkpoint_id))
            .map(|phys| {
                (
                    phys.primary.as_ref().unwrap().clone(),
                    phys.disk,
                    phys.cache_guid,
                )
            })
            .ok_or_else(|| anyhow!("Primary Superblock not found"))?;

        // XXX proper error handling
        for phys in results.iter().flatten() {
            assert_eq!(phys.cache_guid, cache_guid);
            assert!(phys.disk != primary_disk || phys.primary.is_some());
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
