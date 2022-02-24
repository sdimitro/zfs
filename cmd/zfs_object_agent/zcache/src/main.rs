#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
mod clear_hit_data;
mod iostat;
mod list_devices;
mod remote_channel;
mod report_hits;
mod stats;
mod subcommand;

use anyhow::Result;
use clap::AppSettings;
use clap::Arg;
use clear_hit_data::ClearHitData;
use iostat::IoStat;
use list_devices::ListDevices;
use log::*;
use report_hits::ReportHits;
use stats::Stats;
use subcommand::ZcacheSubCommand;
use util::writeln_stderr;

fn main() -> Result<()> {
    async_main()
}

#[tokio::main]
async fn async_main() -> Result<()> {
    // Store sub-command structures in a vector
    // When adding a new sub-command:
    // 1. Create a new module that implements the sub-command with the ZcacheSubCommand trait
    // 2. Add an entry here to add an instance of the new sub-command to the sub_commands vector
    let sub_commands: Vec<Box<dyn ZcacheSubCommand>> = vec![
        Box::new(ClearHitData),
        Box::new(IoStat),
        Box::new(ListDevices),
        Box::new(ReportHits),
        Box::new(Stats),
    ];

    // Define global command arguments
    let mut app = clap::App::new("zcache")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .about("ZFS ZettaCache Command")
        .version("1.0")
        .arg(
            Arg::with_name("verbose")
                .global(true)
                .long("verbose")
                .short("v")
                .multiple(true)
                .help("Write verbose output for logging and debugging"),
        )
        .arg(
            Arg::with_name("log-file")
                .requires("verbose")
                .global(true)
                .long("log-file")
                .value_name("FILE")
                .help("File to log debugging output to")
                .takes_value(true),
        );
    // Add in parsing info for sub-commands
    for cmd in &sub_commands {
        app = app.subcommand(cmd.subcommand());
    }

    // Process the command line
    let matches = app.get_matches();

    // Set up logging macros
    util::setup_logging(
        matches.occurrences_of("verbose"),
        matches.value_of("log-file"),
        None,
        true,
    );

    // Search for and invoke the appropriate sub-command
    let (cmd_name, cmd_args) = matches.subcommand();
    match sub_commands.into_iter().find(|cmd| cmd.name() == cmd_name) {
        Some(mut subcmd) => {
            if let Err(e) = subcmd.invoke(cmd_args.unwrap()).await {
                writeln_stderr!("{:?}", e);
                std::process::exit(1);
            }
        }
        None => {
            writeln_stderr!("Unable to invoke {}", cmd_name);
            writeln_stderr!("{}", matches.usage());
            std::process::exit(exitcode::USAGE);
        }
    }
    Ok(())
}
