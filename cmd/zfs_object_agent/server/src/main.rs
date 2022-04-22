use std::time::Duration;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use git_version::git_version;
use log::*;
use util::tunable;
use util::writeln_stderr;
use util::TrackingAllocator;
use util::ALLOCATOR_PRINT_MIN_ALLOCS;
use util::ALLOCATOR_PRINT_MIN_BYTES;
use zettaobject::test_connectivity;

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

static GIT_VERSION: &str = git_version!(
    fallback = match option_env!("CARGO_ZOA_GITREV") {
        Some(value) => value,
        None => "unknown",
    }
);

tunable! {
    static ref ALLOCATOR_PRINT_DURATION: Duration = Duration::from_secs(60);
}

#[derive(Args)]
struct LoggingArgs {
    /// Sets the level of logging verbosity
    #[clap(short = 'v', parse(from_occurrences))]
    verbosity: u64,

    /// File to log output to
    #[clap(short = 'o', long, value_name = "FILE")]
    output_file: Option<String>,

    /// Logging configuration yaml file
    #[clap(
        short = 'l',
        long,
        value_name = "FILE",
        conflicts_with = "output-file",
        conflicts_with = "verbosity"
    )]
    log_config: Option<String>,
}

#[derive(Parser)]
#[clap(version=GIT_VERSION)]
#[clap(name = "ZFS Object Agent")]
#[clap(about = "Enables the ZFS kernel module talk to S3-protocol object storage")]
#[clap(propagate_version = true)]
struct Cli {
    #[clap(flatten)]
    logging: LoggingArgs,

    /// Configuration file to set tunables (toml/json/yaml)
    #[clap(short = 't', long, value_name = "FILE")]
    config_file: Option<String>,

    /// Directory for unix-domain sockets
    #[clap(short = 'd', long, value_name = "DIR", default_value = "/etc/zfs")]
    socket_dir: String,

    /// File/device to use for ZettaCache
    #[clap(short = 'c', long, value_name = "PATH")]
    cache_device: Vec<String>,

    /// Clear the cache when it has incompatible features
    #[clap(long)]
    clear_incompatible_cache: bool,

    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// test connectivity
    #[clap(alias = "test_connectivity")]
    TestConnectivity {
        /// S3 endpoint
        #[clap(short = 'e', long)]
        endpoint: String,

        /// S3 region
        #[clap(short = 'r', long)]
        region: String,

        /// S3 bucket
        #[clap(short = 'b', long)]
        bucket: String,

        /// AWS access key id
        #[clap(
            short = 'i',
            long,
            alias = "aws_access_key_id",
            requires = "aws-secret-access-key",
            required_unless_present = "aws-instance-profile",
            conflicts_with = "aws-instance-profile"
        )]
        aws_access_key_id: Option<String>,

        /// AWS secret access key
        #[clap(
            short = 's',
            long,
            alias = "aws_secret_access_key",
            requires = "aws-access-key-id",
            required_unless_present = "aws-instance-profile",
            conflicts_with = "aws-instance-profile"
        )]
        aws_secret_access_key: Option<String>,

        /// Use AWS instance profile
        #[clap(long, alias = "aws_instance_profile")]
        aws_instance_profile: bool,
    },
}

fn setup_logging(logging: LoggingArgs) {
    util::setup_logging(
        logging.verbosity,
        logging.output_file.as_deref(),
        logging.log_config.as_deref(),
        false,
    );
}

fn main() {
    let cli = Cli::parse();

    if let Some(file_name) = cli.config_file {
        if let Err(error) = util::tunable::read_config(&file_name) {
            writeln_stderr!("error: reading config: {}", error);
            std::process::exit(1);
        }
    }
    if cli.command.is_none() || cli.logging.verbosity > 0 {
        setup_logging(cli.logging);
    }

    match cli.command {
        Some(Commands::TestConnectivity {
            endpoint,
            region,
            bucket,
            aws_access_key_id,
            aws_secret_access_key,
            aws_instance_profile,
        }) => test_connectivity::test_connectivity(
            endpoint,
            region,
            bucket,
            aws_access_key_id,
            aws_secret_access_key,
            aws_instance_profile,
        ),

        None => {
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
                    debug!("measurements:\n{}", util::measure::dump());
                }
            });

            match zettaobject::init::start(
                &cli.socket_dir,
                cli.cache_device.iter().map(AsRef::as_ref).collect(),
                cli.clear_incompatible_cache,
                runtime,
            ) {
                Ok(()) => panic!("unreachable statement"),
                Err(err) => writeln_stderr!("error: couldn't start server: {}", err),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn neg(s: &str) {
        assert!(Cli::try_parse_from(s.split_whitespace()).is_err());
    }

    fn pos(s: &str) -> Cli {
        Cli::try_parse_from(s.split_whitespace()).unwrap()
    }

    #[test]
    fn verbosity() {
        assert_eq!(pos("zfs_object_agent").logging.verbosity, 0);
        assert_eq!(pos("zfs_object_agent -v").logging.verbosity, 1);
        assert_eq!(pos("zfs_object_agent -v -v").logging.verbosity, 2);
        assert_eq!(pos("zfs_object_agent -vv").logging.verbosity, 2);
    }

    #[test]
    fn log_conflict() {
        neg("zfs_object_agent -l -v");
        neg("zfs_object_agent -l --output-file foo");
    }

    #[test]
    fn test_connectivity_missing() {
        neg("zfs_object_agent test_connectivity");
        neg("zfs_object_agent test-connectivity");
        neg("zfs_object_agent test-connectivity -e foo -r bar -b baz");
    }

    #[test]
    fn test_connectivity_profile() {
        let cli =
            pos("zfs_object_agent test_connectivity -e foo -r bar -b baz --aws_instance_profile");
        match cli.command {
            Some(Commands::TestConnectivity {
                endpoint,
                region,
                bucket,
                aws_access_key_id,
                aws_secret_access_key,
                aws_instance_profile,
            }) => {
                assert_eq!(&endpoint, "foo");
                assert_eq!(&region, "bar");
                assert_eq!(&bucket, "baz");
                assert!(aws_access_key_id.is_none());
                assert!(aws_secret_access_key.is_none());
                assert!(aws_instance_profile);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn test_connectivity_creds() {
        let cli = pos("zfs_object_agent test_connectivity -e foo -r bar -b baz -i abcd -s 1234");
        match cli.command {
            Some(Commands::TestConnectivity {
                endpoint,
                region,
                bucket,
                aws_access_key_id,
                aws_secret_access_key,
                aws_instance_profile,
            }) => {
                assert_eq!(&endpoint, "foo");
                assert_eq!(&region, "bar");
                assert_eq!(&bucket, "baz");
                assert_eq!(aws_access_key_id.unwrap(), "abcd");
                assert_eq!(aws_secret_access_key.unwrap(), "1234");
                assert!(!aws_instance_profile);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn test_connectivity_neg() {
        neg("zfs_object_agent test_connectivity -e foo -r bar -b baz -i abcd");
        neg("zfs_object_agent test_connectivity -e foo -r bar -b baz -i abcd -s 1234 --aws_instance_profile");
        neg("zfs_object_agent test_connectivity -e foo -r bar -b baz -i abcd --aws_instance_profile");
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
