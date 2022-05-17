use std::path::PathBuf;

use crate::zettacache::zcdb::ZCacheDBHandle;
use crate::CacheOpenMode;

// This file and its data structures exists solely to interact with the zcachedb
// binary generated from the zcdb crate/directory. This way we don't have to
// unnecessarily expose structures like BlockAllocator outside the zettacache
// crate through lib.rs.
//
// TODO: Command that gives a frequency count for each key in the operation
// log (e.g. a histogram where the x-axis is the keys and the y-axis is the
// number of entries)
//
// TODO: Command for stats for blockbased logs, printing the average chunk
// size in bytes and the number of entries (next_chunk_offset can tell us
// how big it is).
//
// TODO: Command that prints a histogram of number of blocks of each
// blocksize. Note that this is slightly different than # block in each slab
// bucket, since extent slabs can have different size blocks in them. We
// could use the index for that since the block allocator wouldn't know the
// exact block sizes by that point.
//
// TODO: Command that calculates the condensed size of the existing
// spacemaps and compares it to their actual size.
//
// TODO: Leak detection and consistency checks between the block allocator
// and the index.
//
// TODO: Ping Mark for any more command ideas that would be helpful for
// debugging the index.
#[derive(Debug)]
pub enum ZettaCacheDBCommand {
    // TODO: We still need options to explicitly iterate over the index,
    // operation_log, merging_operation, etc..
    // TODO: Need verification option for these structures (e.g. print out error
    // if there is a double-ALLOC on a spacemap).
    DumpStructures(DumpStructuresOptions),
    DumpSlabs(DumpSlabsOptions),
    DumpSpaceUsage,
    DumpSuperblocks,
    VerifyIndex,
}

#[derive(Debug)]
pub struct DumpStructuresOptions {
    pub dump_defaults: bool,
    pub dump_spacemaps: bool,
    pub dump_operation_log_raw: bool,
    pub dump_index_log_raw: bool,
    pub dump_rebalance_log_raw: bool,
    pub dump_atime_histogram: bool,
}

impl Default for DumpStructuresOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl DumpStructuresOptions {
    pub fn new() -> Self {
        DumpStructuresOptions {
            dump_defaults: true,
            dump_spacemaps: false,
            dump_operation_log_raw: false,
            dump_index_log_raw: false,
            dump_rebalance_log_raw: false,
            dump_atime_histogram: false,
        }
    }

    pub fn defaults(mut self, value: bool) -> Self {
        self.dump_defaults = value;
        self
    }

    pub fn spacemaps(mut self, value: bool) -> Self {
        self.dump_spacemaps = value;
        self
    }

    pub fn operation_log_raw(mut self, value: bool) -> Self {
        self.dump_operation_log_raw = value;
        self
    }

    pub fn index_log_raw(mut self, value: bool) -> Self {
        self.dump_index_log_raw = value;
        self
    }

    pub fn rebalance_log_raw(mut self, value: bool) -> Self {
        self.dump_rebalance_log_raw = value;
        self
    }

    pub fn atime_histogram(mut self, value: bool) -> Self {
        self.dump_atime_histogram = value;
        self
    }
}

#[derive(Debug)]
pub struct DumpSlabsOptions {
    pub verbosity: u64,
}

impl Default for DumpSlabsOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl DumpSlabsOptions {
    pub fn new() -> Self {
        DumpSlabsOptions { verbosity: 0 }
    }

    pub fn verbosity(mut self, value: u64) -> Self {
        self.verbosity = value;
        self
    }
}

impl ZettaCacheDBCommand {
    pub async fn issue_command(
        command: ZettaCacheDBCommand,
        mode: CacheOpenMode,
    ) -> Result<(), anyhow::Error> {
        let paths = mode.device_paths().await?;
        match command {
            ZettaCacheDBCommand::DumpSuperblocks => ZCacheDBHandle::dump_superblocks(paths).await,
            _ => ZettaCacheDBCommand::issue_pool_state_command(command, paths).await,
        }
    }

    async fn issue_pool_state_command(
        command: ZettaCacheDBCommand,
        paths: Vec<PathBuf>,
    ) -> Result<(), anyhow::Error> {
        let mut handle = ZCacheDBHandle::open(paths).await?;
        match command {
            ZettaCacheDBCommand::DumpStructures(opts) => handle.dump_structures(opts).await,
            ZettaCacheDBCommand::DumpSlabs(opts) => handle.dump_slabs(opts).await,
            ZettaCacheDBCommand::DumpSpaceUsage => handle.dump_space().await,
            ZettaCacheDBCommand::VerifyIndex => handle.verify_index().await,
            ZettaCacheDBCommand::DumpSuperblocks => {
                panic!("non-applicable command after opening whole pool state")
            }
        }
        Ok(())
    }
}
