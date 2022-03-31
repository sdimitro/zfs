use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use util::nice_p2size;
use util::writeln_stderr;
use util::writeln_stdout;

use super::ZettaCheckpointPhys;
use crate::base_types::DiskId;
use crate::block_access::BlockAccess;
use crate::block_access::Disk;
use crate::block_allocator::zcdb::zcachedb_dump_slabs;
use crate::block_allocator::zcdb::zcachedb_dump_spacemaps;
use crate::extent_allocator::ExtentAllocator;
use crate::extent_allocator::ExtentAllocatorBuilder;
use crate::superblock::PrimaryPhys;
use crate::superblock::SuperblockPhys;
use crate::DumpSlabsOptions;
use crate::DumpStructuresOptions;

pub struct ZCacheDBHandle {
    block_access: Arc<BlockAccess>,
    primary: PrimaryPhys,
    primary_disk: DiskId,
    guid: u64,
    checkpoint: Arc<ZettaCheckpointPhys>,
    extent_allocator: Arc<ExtentAllocator>,
}

impl ZCacheDBHandle {
    pub async fn dump_superblocks(paths: Vec<&str>) -> Result<()> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in paths {
            match Disk::new(path, true) {
                Ok(disk) => disks.push(disk),
                Err(err) => writeln_stderr!("error: {}", err),
            }
        }
        if disks.is_empty() {
            return Ok(());
        }
        let block_access = BlockAccess::new(disks, true);
        SuperblockPhys::dump_all(&block_access).await;
        Ok(())
    }

    pub async fn open(paths: Vec<&str>) -> Result<ZCacheDBHandle> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in paths {
            disks.push(Disk::new(path, true)?);
        }
        let block_access = Arc::new(BlockAccess::new(disks, true));

        let (primary, primary_disk, guid, _extra_disks) = PrimaryPhys::read(&block_access).await?;
        let checkpoint =
            Arc::new(ZettaCheckpointPhys::read(&block_access, primary.checkpoint).await);

        let mut builder = ExtentAllocatorBuilder::new(&checkpoint.extent_allocator);
        // We should be able to get away without claiming the metadata space,
        // since we aren't allocating anything, but we may also want to do this
        // for verification (e.g. that there aren't overlapping Extents).
        checkpoint.claim(&mut builder);
        let extent_allocator = Arc::new(ExtentAllocator::open(builder));

        Ok(ZCacheDBHandle {
            block_access,
            primary,
            primary_disk,
            guid,
            checkpoint,
            extent_allocator,
        })
    }

    pub async fn dump_free_space(&self) {
        writeln_stdout!("Superblock");
        writeln_stdout!("  Primary {:?}, GUID: {}", self.primary_disk, self.guid);
        writeln_stdout!();

        writeln_stdout!("Checkpoint Region");
        writeln_stdout!("  {:?}", self.primary.checkpoint_capacity);
        writeln_stdout!(
            "  checkpoint: {} used out of {} ({:.1}%, must be <50%)",
            nice_p2size(self.primary.checkpoint.size),
            nice_p2size(self.primary.checkpoint_capacity.size),
            self.primary.checkpoint.size as f64 * 100.0
                / self.primary.checkpoint_capacity.size as f64
        );
        writeln_stdout!();

        writeln_stdout!("Old Checkpoint Regions");
        let mut unused_checkpoint_space = 0;
        for region in self.primary.old_checkpoint_capacity.iter() {
            unused_checkpoint_space += region.size;
            writeln_stdout!("  {:?}", region);
        }
        writeln_stdout!("  ----------------------");
        writeln_stdout!("  total: {}", nice_p2size(unused_checkpoint_space));
        writeln_stdout!();

        writeln_stdout!("Metadata Region");
        let mut total_used_bytes = 0;
        let mut total_allocated_bytes = 0;
        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "operation log",
            nice_p2size(self.checkpoint.operation_log.bytes()),
            nice_p2size(self.checkpoint.operation_log.capacity_bytes())
        );
        total_used_bytes += self.checkpoint.operation_log.bytes();
        total_allocated_bytes += self.checkpoint.operation_log.capacity_bytes();

        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "spacemap",
            nice_p2size(self.checkpoint.block_allocator.spacemap_bytes()),
            nice_p2size(self.checkpoint.block_allocator.spacemap_capacity_bytes())
        );
        total_used_bytes += self.checkpoint.block_allocator.spacemap_bytes();
        total_allocated_bytes += self.checkpoint.block_allocator.spacemap_capacity_bytes();

        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "spacemap_next",
            nice_p2size(self.checkpoint.block_allocator.spacemap_next_bytes()),
            nice_p2size(
                self.checkpoint
                    .block_allocator
                    .spacemap_next_capacity_bytes()
            )
        );
        total_used_bytes += self.checkpoint.block_allocator.spacemap_next_bytes();
        total_allocated_bytes += self
            .checkpoint
            .block_allocator
            .spacemap_next_capacity_bytes();

        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "index log",
            nice_p2size(self.checkpoint.old_index.log_bytes()),
            nice_p2size(self.checkpoint.old_index.log_capacity_bytes())
        );
        total_used_bytes += self.checkpoint.old_index.log_bytes();
        total_allocated_bytes += self.checkpoint.old_index.log_capacity_bytes();

        if let Some(progress) = self.checkpoint.merge_progress.clone() {
            writeln_stdout!(
                "  {:>13} - {:>6} used out of {:>6} allocated",
                "progress log",
                nice_p2size(progress.operation_log.bytes()),
                nice_p2size(progress.operation_log.capacity_bytes())
            );
            total_used_bytes += progress.operation_log.bytes();
            total_allocated_bytes += progress.operation_log.capacity_bytes();
            writeln_stdout!(
                "  {:>13} - {:>6} used out of {:>6} allocated",
                "progress index",
                nice_p2size(progress.new_index.log_bytes()),
                nice_p2size(progress.new_index.log_capacity_bytes())
            );
            total_used_bytes += progress.new_index.log_bytes();
            total_allocated_bytes += progress.new_index.log_capacity_bytes();
        }
        writeln_stdout!("  ----------------------");
        let metadata_region_size = self
            .checkpoint
            .extent_allocator
            .capacity
            .iter()
            .map(|extent| extent.size)
            .sum();
        writeln_stdout!(
            "  {:>13} - {} ({:.1}%) used, {} ({:.1}%) allocated out of {:>6} total",
            "total",
            nice_p2size(total_used_bytes),
            total_used_bytes as f64 * 100.0 / metadata_region_size as f64,
            nice_p2size(total_allocated_bytes),
            total_allocated_bytes as f64 * 100.0 / metadata_region_size as f64,
            nice_p2size(metadata_region_size)
        );
        writeln_stdout!("  ----------------------");
        for (disk, (used, total)) in self.extent_allocator.zcachedb_metadata_per_disk() {
            writeln_stdout!(
                "  {:?} - {:>6} allocated out of {:>6} total",
                disk,
                nice_p2size(used),
                nice_p2size(total)
            );
        }
        writeln_stdout!();

        let balloc_size = self
            .checkpoint
            .block_allocator
            .capacity()
            .iter()
            .map(|extent| extent.size)
            .sum();
        writeln_stdout!("{:>6} User Data Region", nice_p2size(balloc_size));
    }

    pub async fn dump_structures(&self, opts: DumpStructuresOptions) {
        if opts.dump_defaults {
            writeln_stdout!("{:#?}", self.primary);
            writeln_stdout!("{:#?}", self.checkpoint);
        }

        if opts.dump_atime_histogram {
            writeln_stdout!("DUMP INDEX ATIME HISTOGRAM");
            writeln_stdout!("{}", self.checkpoint.old_index.atime_histogram());

            if let Some(progress) = &self.checkpoint.merge_progress {
                writeln_stdout!("DUMP MERGE INDEX ATIME HISTOGRAM");
                writeln_stdout!("{}", progress.new_index.atime_histogram());
            }
        }

        if opts.dump_spacemaps {
            zcachedb_dump_spacemaps(
                self.checkpoint.block_allocator.clone(),
                self.block_access.clone(),
                self.extent_allocator.clone(),
            )
            .await;
        }

        if opts.dump_operation_log_raw {
            self.checkpoint
                .operation_log
                .iter_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;
            if let Some(mpp) = &self.checkpoint.merge_progress {
                writeln_stdout!("\nold operation log from MergeProgressPhys:");
                mpp.operation_log
                    .iter_chunks(self.block_access.clone())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
            }
        }

        if opts.dump_index_log_raw {
            self.checkpoint
                .old_index
                .iter_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;

            self.checkpoint
                .old_index
                .iter_summary_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;

            if let Some(mpp) = &self.checkpoint.merge_progress {
                writeln_stdout!("\nnew index from MergeProgressPhys:");
                mpp.new_index
                    .iter_chunks(self.block_access.clone())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
                mpp.new_index
                    .iter_summary_chunks(self.block_access.clone())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
            }
        }

        if opts.dump_rebalance_log_raw {
            if let Some(progress) = &self.checkpoint.merge_progress {
                if let Some(log) = progress.rebalance_log.as_ref() {
                    log.iter_chunks(self.block_access.clone())
                        .for_each(|chunk| async move {
                            writeln_stdout!("{:#?}", chunk);
                        })
                        .await;
                }
            }
        }
    }

    pub async fn dump_slabs(&self, opts: DumpSlabsOptions) {
        zcachedb_dump_slabs(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            self.checkpoint.block_allocator.clone(),
            opts,
        )
        .await;
    }

    pub async fn verify_index(&self) {
        writeln_stdout!("iterating current (old) index to verify histogram...");
        self.checkpoint
            .old_index
            .verify_histogram(self.block_access.clone())
            .await;

        if let Some(mpp) = &self.checkpoint.merge_progress {
            writeln_stdout!("iterating merge (new) index to verify histogram...");
            mpp.new_index
                .verify_histogram(self.block_access.clone())
                .await;
        }
        writeln_stdout!("histograms correct");
    }
}
