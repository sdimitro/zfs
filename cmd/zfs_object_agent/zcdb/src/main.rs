use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;
use git_version::git_version;
use zettacache::CacheOpenMode;
use zettacache::DumpSlabsOptions;
use zettacache::DumpStructuresOptions;
use zettacache::ZettaCacheDBCommand;

static GIT_VERSION: &str = git_version!(
    fallback = match option_env!("CARGO_ZOA_GITREV") {
        Some(value) => value,
        None => "unknown",
    }
);

#[derive(Parser)]
#[clap(version=GIT_VERSION)]
#[clap(name = "zcachedb")]
#[clap(about = "ZFS ZettaCache Debugger")]
#[clap(propagate_version = true)]
struct Cli {
    /// File/device to use for ZettaCache
    #[clap(short = 'c', long, value_name = "PATH")]
    cache_device: Option<Vec<PathBuf>>,

    /// Directory path to use for importing devices that are part of the
    /// ZettaCache
    #[clap(
        short = 'd',
        long,
        value_name = "DIR",
        conflicts_with = "cache-device",
        default_value = "/dev/disk/by-id/"
    )]
    cache_device_dir: PathBuf,

    /// Sets the verbosity level for logging and debugging
    #[clap(short = 'v', long, parse(from_occurrences), global = true)]
    verbose: u64,

    /// File to log debugging output to
    #[clap(long, requires = "verbose", value_name = "FILE", global = true)]
    log_file: Option<PathBuf>,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// print out on-disk structures
    Logs {
        /// skip all structures that are printed by default
        #[clap(short = 'n', long)]
        nodefaults: bool,

        /// dump block allocator spacemaps
        #[clap(long)]
        spacemap: bool,

        /// dump operation log
        #[clap(long)]
        operation: bool,

        /// dump index log
        #[clap(long)]
        index: bool,

        /// dump rebalance log
        #[clap(long)]
        rebalance: bool,

        /// dump atime histogram of index
        #[clap(long)]
        atime_histogram: bool,
    },

    /// dump the superblock contents of the specified disks
    Superblocks,

    /// dump slab info
    Slabs {
        /// Sets the level of information
        #[clap(short = 'i', parse(from_occurrences))]
        detail: u64,
    },

    /// dump space usage statistics
    Space,

    /// verify index histogram
    Index,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // When zcachedb is used in UNIX shell pipeline and its output is not fully
    // consumed a SIGPIPE (e.g. "broken pipe") signal is sent to us. By default,
    // we would abort and generate a core dump which is annoying. The unsafe
    // line below changes that behavior to just terminating as it is expected by
    // other UNIX utilities.
    // reference: https://github.com/rust-lang/rust/issues/46016
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    // Set up logging macros
    util::setup_logging(cli.verbose, cli.log_file.as_deref(), None, true);

    // Set up cache paths
    let cache_mode = match cli.cache_device {
        Some(paths) => CacheOpenMode::DeviceList(paths),
        None => CacheOpenMode::DiscoveryDirectory(cli.cache_device_dir),
    };

    match cli.command {
        Commands::Logs {
            nodefaults,
            spacemap: spacemaps,
            operation: operation_log_raw,
            index: index_log_raw,
            rebalance: rebalance_log_raw,
            atime_histogram,
        } => {
            ZettaCacheDBCommand::issue_command(
                ZettaCacheDBCommand::DumpStructures(
                    DumpStructuresOptions::default()
                        .defaults(!nodefaults)
                        .spacemaps(spacemaps)
                        .operation_log_raw(operation_log_raw)
                        .index_log_raw(index_log_raw)
                        .rebalance_log_raw(rebalance_log_raw)
                        .atime_histogram(atime_histogram),
                ),
                cache_mode,
            )
            .await
        }
        Commands::Superblocks => {
            ZettaCacheDBCommand::issue_command(ZettaCacheDBCommand::DumpSuperblocks, cache_mode)
                .await
        }
        Commands::Slabs { detail } => {
            ZettaCacheDBCommand::issue_command(
                ZettaCacheDBCommand::DumpSlabs(DumpSlabsOptions { verbosity: detail }),
                cache_mode,
            )
            .await
        }
        Commands::Space => {
            ZettaCacheDBCommand::issue_command(ZettaCacheDBCommand::DumpSpaceUsage, cache_mode)
                .await
        }
        Commands::Index => {
            ZettaCacheDBCommand::issue_command(ZettaCacheDBCommand::VerifyIndex, cache_mode).await
        }
    }
}

#[cfg(test)]
mod test_clap {
    use clap::IntoApp;

    use super::*;

    #[test]
    fn test_debug_asserts() {
        Cli::command().debug_assert();
    }
}
