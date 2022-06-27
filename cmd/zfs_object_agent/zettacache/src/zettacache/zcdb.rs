use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use util::nice_p2size;
use util::writeln_stderr;
use util::writeln_stdout;

use super::CheckpointPhys;
use crate::base_types::CacheGuid;
use crate::base_types::DiskId;
use crate::block_access::BlockAccess;
use crate::block_access::Disk;
use crate::block_allocator::zcdb::zcachedb_dump_slabs;
use crate::block_allocator::zcdb::zcachedb_dump_spacemaps;
use crate::features::check_features;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::superblock::PrimaryPhys;
use crate::superblock::SuperblockPhys;
use crate::superblock::SUPERBLOCK_SIZE;
use crate::CacheOpenError;
use crate::DumpSlabsOptions;
use crate::DumpStructuresOptions;

pub struct ZCacheDBHandle {
    block_access: Arc<BlockAccess>,
    primary: PrimaryPhys,
    primary_disk: DiskId,
    guid: CacheGuid,
    checkpoint: Arc<CheckpointPhys>,
    slab_builder: SlabAllocatorBuilder,
}

impl ZCacheDBHandle {
    pub async fn dump_superblocks(paths: Vec<PathBuf>) -> Result<()> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in paths {
            match Disk::new(&path, true) {
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

    pub async fn open(paths: Vec<PathBuf>) -> Result<ZCacheDBHandle> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in &paths {
            disks.push(Disk::new(path, true)?);
        }
        let block_access = Arc::new(BlockAccess::new(disks, true));

        let feature_flags = PrimaryPhys::read_features(&block_access).await?;
        check_features(&feature_flags)
            .map_err(|e| CacheOpenError::IncompatibleFeatures(paths, e))?;

        let (primary, primary_disk, guid, _extra_disks) = PrimaryPhys::read(&block_access).await?;
        let checkpoint = Arc::new(CheckpointPhys::read(&block_access, &primary.checkpoint).await?);

        let mut slab_builder = SlabAllocatorBuilder::new(checkpoint.slab_allocator.clone());
        // We should be able to get away without claiming the metadata space,
        // since we aren't allocating anything, but we may also want to do this
        // for verification (e.g. that there aren't overlapping Extents).
        checkpoint.claim(&mut slab_builder);

        Ok(ZCacheDBHandle {
            block_access,
            primary,
            primary_disk,
            guid,
            checkpoint,
            slab_builder,
        })
    }

    pub async fn dump_space(&self) {
        writeln_stdout!("Superblock: {}", nice_p2size(SUPERBLOCK_SIZE));
        writeln_stdout!("  Primary {:?}, {:?}", self.primary_disk, self.guid);
        writeln_stdout!();

        let slabs_capacity = self
            .checkpoint
            .slab_allocator
            .capacity()
            .iter()
            .map(|extent| extent.size)
            .sum();
        writeln_stdout!("Slabs Region: {}", nice_p2size(slabs_capacity));
        writeln_stdout!("-------------------------------");
        let mut total_used_bytes = 0;
        let mut print_meta = |name: &str, space: u64| {
            writeln_stdout!("  {name:>20}:  {:>6}", nice_p2size(space));
            total_used_bytes += space;
        };

        print_meta(
            "checkpoint",
            self.primary.checkpoint.iter().map(|e| e.size).sum(),
        );
        print_meta("spacemap", self.checkpoint.block_allocator.spacemap_bytes());
        print_meta(
            "next spacemap",
            self.checkpoint.block_allocator.spacemap_next_bytes(),
        );
        print_meta("index", self.checkpoint.old_index.num_bytes());
        print_meta("operation log", self.checkpoint.operation_log.bytes());

        if let Some(progress) = self.checkpoint.merge_progress.clone() {
            print_meta("next index", progress.new_index.num_bytes());
            print_meta("next operation log", progress.operation_log.bytes());
        }
        writeln_stdout!("-------------------------------");
        writeln_stdout!(
            "  {:>20}:  {:>6} ({:.1}%)",
            "total metadata",
            nice_p2size(total_used_bytes),
            total_used_bytes as f64 * 100.0 / slabs_capacity as f64,
        );
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
                &self.slab_builder,
            )
            .await;
        }

        if opts.dump_operation_log_raw {
            self.checkpoint
                .operation_log
                .iter_chunks(self.block_access.clone(), self.slab_builder.access())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;
            if let Some(mpp) = &self.checkpoint.merge_progress {
                writeln_stdout!("\nold operation log from MergeProgressPhys:");
                mpp.operation_log
                    .iter_chunks(self.block_access.clone(), self.slab_builder.access())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
            }
        }

        if opts.dump_index_log_raw {
            self.checkpoint
                .old_index
                .iter_chunks(self.block_access.clone(), self.slab_builder.access())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;

            self.checkpoint
                .old_index
                .iter_summary_chunks(self.block_access.clone(), self.slab_builder.access())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;

            if let Some(mpp) = &self.checkpoint.merge_progress {
                writeln_stdout!("\nnew index from MergeProgressPhys:");
                mpp.new_index
                    .iter_chunks(self.block_access.clone(), self.slab_builder.access())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
                mpp.new_index
                    .iter_summary_chunks(self.block_access.clone(), self.slab_builder.access())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
            }
        }

        if opts.dump_rebalance_log_raw {
            if let Some(progress) = &self.checkpoint.merge_progress {
                if let Some(log) = progress.rebalance_log.as_ref() {
                    log.iter_chunks(self.block_access.clone(), self.slab_builder.access())
                        .for_each(|chunk| async move {
                            writeln_stdout!("{:#?}", chunk);
                        })
                        .await;
                }
            }
        }
    }

    pub async fn dump_slabs(&mut self, opts: DumpSlabsOptions) {
        zcachedb_dump_slabs(
            self.block_access.clone(),
            self.slab_builder.access(),
            self.checkpoint.block_allocator.clone(),
            opts,
        )
        .await;
    }

    pub async fn verify_index(&self) {
        writeln_stdout!("iterating current (old) index to verify histogram...");
        self.checkpoint
            .old_index
            .verify_histogram(self.block_access.clone(), self.slab_builder.access())
            .await;

        if let Some(mpp) = &self.checkpoint.merge_progress {
            writeln_stdout!("iterating merge (new) index to verify histogram...");
            mpp.new_index
                .verify_histogram(self.block_access.clone(), self.slab_builder.access())
                .await;
        }
        writeln_stdout!("histograms correct");
    }
}
