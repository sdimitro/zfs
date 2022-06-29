#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]

mod add;
mod expand;
mod hits;
mod iostat;
mod labelclear;
mod list;
mod remote_channel;
mod stats;
mod status;
mod subcommand;
mod sync;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use hits::ClearHitData;
use log::*;
use subcommand::ZcacheSubCommand;

fn main() -> Result<()> {
    async_main()
}

#[derive(Parser)]
// XXX other commands use a git derived version here
#[clap(version = "1.1")]
#[clap(name = "zcache")]
#[clap(about = "ZFS ZettaCache Command")]
#[clap(propagate_version = true)]
struct Cli {
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
/// Sub-commands for `zcache`. When adding a new sub-command:
/// 1. Create a new module that implements the sub-command with the `ZcacheSubCommand` trait
/// and describes the derived sub-command arguments in the struct.
/// 2. Add an entry to the enum here where it will be parsed and instantiated automatically.
/// 3. Add a match entry in `Commands::as_trait()`
enum Commands {
    Hits(hits::Hits),
    Iostat(iostat::Iostat),
    List(list::List),
    Stats(stats::Stats),
    Add(add::Add),
    Expand(expand::Expand),
    Sync(sync::Sync),
    Labelclear(labelclear::Labelclear),
    Status(status::Status),

    // clear_hit_data is deprecated/hidden
    #[clap(rename_all = "snake_case")]
    ClearHitData(ClearHitData),
}

impl Commands {
    fn as_trait(&self) -> &dyn ZcacheSubCommand {
        match self {
            Commands::Hits(hits) => hits,
            Commands::Iostat(iostat) => iostat,
            Commands::List(list) => list,
            Commands::Stats(stats) => stats,
            Commands::Add(add) => add,
            Commands::Expand(expand) => expand,
            Commands::Sync(sync) => sync,
            Commands::Labelclear(labelclear) => labelclear,
            Commands::ClearHitData(clear_hit_data) => clear_hit_data,
            Commands::Status(status) => status,
        }
    }
}

#[tokio::main]
async fn async_main() -> Result<()> {
    let cli = Cli::parse();

    // Set up logging macros
    util::setup_logging(cli.verbose, cli.log_file.as_deref(), None, true);

    cli.command.as_trait().invoke().await
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
