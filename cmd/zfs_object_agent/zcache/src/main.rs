#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]
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

fn main() -> Result<()> {
    // When zcache is used in a UNIX shell pipeline and its output is not fully
    // consumed a SIGPIPE (e.g. "broken pipe") signal is sent to us. By default,
    // we would abort and generate a core dump which is annoying. The unsafe
    // line below changes that behavior to just terminating as it is expected by
    // other UNIX utilities.
    // reference: https://github.com/rust-lang/rust/issues/46016
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
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
        Some(mut subcmd) => subcmd.invoke(cmd_args.unwrap()).await?,
        None => {
            println!("Unable to invoke {}", cmd_name);
            println!("{}", matches.usage());
            std::process::exit(exitcode::USAGE);
        }
    }
    Ok(())
}
