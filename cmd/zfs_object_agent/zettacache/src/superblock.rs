use crate::base_types::*;
use crate::block_access::*;
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

    pub async fn read(block_access: &BlockAccess) -> Result<(Self, DiskId, u64)> {
        let superblocks = SuperblockPhys::read_all(block_access).await?;

        let (primary, primary_disk, guid) = superblocks
            .iter()
            .find_map(|phys| {
                phys.primary
                    .as_ref()
                    .map(|primary| (primary.clone(), phys.disk, phys.guid))
            })
            .ok_or_else(|| anyhow!("Primary Superblock not found"))?;

        for (id, phys) in superblocks.iter().enumerate() {
            // XXX proper error handling
            // XXX we should be able to reorder them?
            assert_eq!(DiskId(id.try_into().unwrap()), phys.disk);
            assert_eq!(phys.guid, guid);
            assert!(phys.primary.is_none() || phys.disk == primary_disk);
        }

        // XXX proper error handling
        assert_eq!(block_access.disks().count(), primary.num_disks);
        assert!(primary.checkpoint_capacity.contains(&primary.checkpoint));

        Ok((primary, primary_disk, guid))
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

    async fn read_all(block_access: &BlockAccess) -> Result<Vec<SuperblockPhys>> {
        block_access
            .disks()
            .map(|disk| SuperblockPhys::read(block_access, disk))
            .collect::<FuturesOrdered<_>>()
            .try_collect()
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
