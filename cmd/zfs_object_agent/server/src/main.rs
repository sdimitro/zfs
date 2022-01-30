use clap::{Arg, SubCommand};
use git_version::git_version;
use lazy_static::lazy_static;
use log::*;
use std::time::Duration;
use util::get_tunable;
use util::TrackingAllocator;
use zettaobject::test_connectivity;

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

static GIT_VERSION: &str = git_version!(
    fallback = match option_env!("CARGO_ZOA_GITREV") {
        Some(value) => value,
        None => "unknown",
    }
);

lazy_static! {
    static ref ALLOCATOR_PRINT_DURATION: Duration =
        Duration::from_secs(get_tunable("allocator_print_secs", 60));
    static ref ALLOCATOR_PRINT_MIN_BYTES: u64 =
        get_tunable("allocator_print_min_bytes", 1024 * 1024);
    static ref ALLOCATOR_PRINT_MIN_ALLOCS: u64 =
        get_tunable("allocator_print_min_allocs", 1_000_000);
}

fn main() {
    let matches = clap::App::new("ZFS Object Agent")
        .about("Enables the ZFS kernel module talk to S3-protocol object storage")
        .version(GIT_VERSION)
        .arg(
            Arg::with_name("verbosity")
                .short("v")
                .multiple(true)
                .help("Sets the level of logging verbosity"),
        )
        .arg(
            Arg::with_name("socket-dir")
                .short("d")
                .long("socket-dir")
                .value_name("DIR")
                .help("Directory for unix-domain sockets")
                .takes_value(true)
                .default_value("/etc/zfs"),
        )
        .arg(
            Arg::with_name("output-file")
                .short("o")
                .long("output-file")
                .value_name("FILE")
                .help("File to log output to")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("cache-device")
                .short("c")
                .long("cache-device")
                .value_name("PATH")
                .help("File/device to use for ZettaCache")
                .takes_value(true)
                .multiple(true)
                .number_of_values(1),
        )
        .arg(
            Arg::with_name("config-file")
                .short("t")
                .long("config-file")
                .value_name("FILE")
                .help("Configuration file to set tunables (toml/json/yaml")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("log-config")
                .short("l")
                .long("log-config")
                .value_name("FILE")
                .help("Logging configuration yaml file")
                .conflicts_with("output-file")
                .conflicts_with("verbosity")
                .takes_value(true),
        )
        .subcommand(
            SubCommand::with_name("test_connectivity")
                .about("test connectivity")
                .arg(
                    Arg::with_name("endpoint")
                        .short("e")
                        .long("endpoint")
                        .help("S3 endpoint")
                        .required(true)
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("region")
                        .short("r")
                        .long("region")
                        .help("S3 region")
                        .required(true)
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("bucket")
                        .short("b")
                        .long("bucket")
                        .help("S3 bucket")
                        .required(true)
                        .takes_value(true),
                )
                .arg(
                    Arg::with_name("aws_access_key_id")
                        .short("i")
                        .long("aws_access_key_id")
                        .takes_value(true)
                        .requires("aws_secret_access_key")
                        .required_unless("aws_instance_profile")
                        .conflicts_with("aws_instance_profile")
                        .help("AWS access key id"),
                )
                .arg(
                    Arg::with_name("aws_secret_access_key")
                        .short("s")
                        .long("aws_secret_access_key")
                        .takes_value(true)
                        .requires("aws_access_key_id")
                        .required_unless("aws_instance_profile")
                        .conflicts_with("aws_instance_profile")
                        .help("AWS secret access key"),
                )
                .arg(
                    Arg::with_name("aws_instance_profile")
                        .long("aws_instance_profile")
                        .takes_value(false)
                        .help("Use AWS instance profile"),
                ),
        )
        .get_matches();

    match matches.subcommand() {
        ("test_connectivity", Some(cmd_options)) => {
            let endpoint = cmd_options.value_of("endpoint").unwrap().to_string();
            let region = cmd_options.value_of("region").unwrap().to_string();
            let bucket = cmd_options.value_of("bucket").unwrap().to_string();
            let aws_access_key_id = cmd_options
                .value_of("aws_access_key_id")
                .map(str::to_string);
            let aws_secret_access_key = cmd_options
                .value_of("aws_secret_access_key")
                .map(str::to_string);
            let aws_instance_profile = cmd_options.is_present("aws_instance_profile");

            test_connectivity::test_connectivity(
                endpoint,
                region,
                bucket,
                aws_access_key_id,
                aws_secret_access_key,
                aws_instance_profile,
            );
        }
        _ => {
            let socket_dir = matches.value_of("socket-dir").unwrap();
            let cache_paths = matches
                .values_of("cache-device")
                .map_or(Vec::new(), |values| values.collect());
            if let Some(file_name) = matches.value_of("config-file") {
                util::read_tunable_config(file_name);
            }

            util::setup_logging(
                matches.occurrences_of("verbosity"),
                matches.value_of("output-file"),
                matches.value_of("log-config"),
                false,
            );

            // This has to be called after setting up tunables.  Allocations
            // that happen before this call will use the defaults hard-coded in
            // alloc.rs
            TrackingAllocator::setup();

            error!(
                "Starting ZFS Object Agent ({}).  Local timezone is {}",
                GIT_VERSION,
                chrono::Local::now().format("%Z (%:z)")
            );

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("zoa")
                .build()
                .unwrap();

            runtime.spawn(async {
                let mut interval = tokio::time::interval(*ALLOCATOR_PRINT_DURATION);
                loop {
                    interval.tick().await;
                    debug!(
                        "allocation tracking:\n{}",
                        TrackingAllocator::format(
                            *ALLOCATOR_PRINT_MIN_ALLOCS,
                            *ALLOCATOR_PRINT_MIN_BYTES
                        )
                    );
                }
            });

            zettaobject::init::start(socket_dir, cache_paths, runtime);
        }
    }
}
