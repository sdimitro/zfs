use std::path::PathBuf;
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
use zettacache::base_types::CacheGuid;
use zettacache::CacheOpenMode;
use zettaobject::object_access::BlobCredentials;
use zettaobject::object_access::ObjectAccessProtocol;
use zettaobject::object_access::S3Credentials;
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
    output_file: Option<PathBuf>,

    /// Logging configuration yaml file
    #[clap(
        short = 'l',
        long,
        value_name = "FILE",
        conflicts_with = "output-file",
        conflicts_with = "verbosity"
    )]
    log_config: Option<PathBuf>,
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
    config_file: Option<PathBuf>,

    /// Directory for unix-domain sockets
    #[clap(short = 'k', long, value_name = "DIR", default_value = "/etc/zfs")]
    socket_dir: PathBuf,

    /// File/device to use for ZettaCache
    #[clap(
        short = 'c',
        long,
        value_name = "PATH",
        conflicts_with = "cache-device-dir"
    )]
    cache_device: Option<Vec<PathBuf>>,

    /// Directory path to use for importing devices that are part of the
    /// ZettaCache
    #[clap(short = 'd', long, value_name = "DIR")]
    cache_device_dir: Option<PathBuf>,

    /// Specific cache GUID to look for in cache device directory
    #[clap(
        short = 'g',
        long,
        value_name = "GUID",
        conflicts_with = "cache-device"
    )]
    guid: Option<CacheGuid>,

    /// Clear the cache when it has incompatible features
    #[clap(long)]
    clear_incompatible_cache: bool,

    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// test connectivity s3
    #[clap(aliases = &["test_connectivity", "test-connectivity"])]
    TestConnectivityS3 {
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

        /// Use IAM instance profile
        #[clap(short = 'm', long, alias = "aws_instance_profile")]
        aws_instance_profile: bool,
    },
    /// test connectivity blob
    TestConnectivityBlob {
        /// Optional Azure-Blob endpoint (for emulator or custom domain)
        #[clap(short = 'e', long)]
        endpoint: Option<String>,

        /// Azure-Blob bucket (a.k.a container)
        #[clap(short = 'b', long)]
        bucket: String,

        /// Azure-Blob account name
        #[clap(short = 'a', long)]
        azure_account: String,

        /// Azure-Blob secret key
        #[clap(
            short = 's',
            long,
            required_unless_present = "managed-identity",
            conflicts_with = "managed-identity"
        )]
        azure_key: Option<String>,

        /// Use managed identity for Azure-Blob
        #[clap(
            short = 'm',
            long,
            required_unless_present = "azure-key",
            conflicts_with = "azure-key"
        )]
        managed_identity: bool,
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zoa")
        .build()
        .unwrap();

    match cli.command {
        Some(Commands::TestConnectivityS3 {
            endpoint,
            region,
            bucket,
            aws_access_key_id,
            aws_secret_access_key,
            aws_instance_profile,
        }) => {
            let protocol = ObjectAccessProtocol::S3 {
                endpoint,
                region,
                credentials: if aws_instance_profile {
                    S3Credentials::InstanceProfile
                } else {
                    S3Credentials::Key {
                        aws_access_key_id: aws_access_key_id.unwrap(),
                        aws_secret_access_key: aws_secret_access_key.unwrap(),
                    }
                },
            };
            runtime.block_on(async move {
                test_connectivity::test_connectivity(protocol, bucket).await
            });
        }
        Some(Commands::TestConnectivityBlob {
            endpoint,
            bucket,
            azure_account,
            azure_key,
            managed_identity,
        }) => {
            let protocol = ObjectAccessProtocol::Blob {
                endpoint,
                credentials: if managed_identity {
                    BlobCredentials::ManagedCredentials { azure_account }
                } else {
                    BlobCredentials::Key {
                        azure_account,
                        azure_key: azure_key.unwrap(),
                    }
                },
            };
            runtime.block_on(async move {
                test_connectivity::test_connectivity(protocol, bucket).await
            });
        }
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

            let cache_mode = match (cli.cache_device, cli.cache_device_dir) {
                (Some(_), Some(_)) => panic!("invalid state"),
                (Some(devices), None) => CacheOpenMode::DeviceList(devices),
                (None, Some(dir)) => CacheOpenMode::DiscoveryDirectory(dir, cli.guid),
                (None, None) => CacheOpenMode::None,
            };

            match zettaobject::init::start(
                &cli.socket_dir,
                cache_mode,
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
    fn test_connectivity_blob_key() {
        let cli = pos(
            "zfs_object_agent test-connectivity-blob -e foo -b bar -a azure-account -s super-secret-key",
        );
        match cli.command {
            Some(Commands::TestConnectivityBlob {
                endpoint,
                bucket,
                azure_account,
                azure_key,
                managed_identity,
            }) => {
                assert_eq!(endpoint.unwrap(), "foo");
                assert_eq!(&bucket, "bar");
                assert_eq!(azure_account, "azure-account");
                assert_eq!(azure_key.unwrap(), "super-secret-key");
                assert!(!managed_identity);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn test_connectivity_blob_managed() {
        let cli = pos(
            "zfs_object_agent test-connectivity-blob -e foo -b bar -a azure-account --managed-identity",
        );
        match cli.command {
            Some(Commands::TestConnectivityBlob {
                endpoint,
                bucket,
                azure_account,
                azure_key,
                managed_identity,
            }) => {
                assert_eq!(endpoint.unwrap(), "foo");
                assert_eq!(&bucket, "bar");
                assert_eq!(azure_account, "azure-account");
                assert!(azure_key.is_none());
                assert!(managed_identity);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn test_connectivity_default_protocol() {
        let cli =
            pos("zfs_object_agent test-connectivity -e foo -r bar -b baz --aws-instance-profile");
        match cli.command {
            Some(Commands::TestConnectivityS3 {
                endpoint,
                region,
                bucket,
                aws_access_key_id,
                aws_secret_access_key,
                aws_instance_profile,
            }) => {
                assert_eq!(endpoint, "foo");
                assert_eq!(region, "bar");
                assert_eq!(&bucket, "baz");
                assert!(aws_access_key_id.is_none());
                assert!(aws_secret_access_key.is_none());
                assert!(aws_instance_profile);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn test_connectivity_profile() {
        let cli = pos(
            "zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz --aws-instance-profile",
        );
        match cli.command {
            Some(Commands::TestConnectivityS3 {
                endpoint,
                region,
                bucket,
                aws_access_key_id,
                aws_secret_access_key,
                aws_instance_profile,
            }) => {
                assert_eq!(endpoint, "foo");
                assert_eq!(region, "bar");
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
        let cli = pos("zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz -i abcd -s 1234");
        match cli.command {
            Some(Commands::TestConnectivityS3 {
                endpoint,
                region,
                bucket,
                aws_access_key_id,
                aws_secret_access_key,
                aws_instance_profile,
            }) => {
                assert_eq!(endpoint, "foo");
                assert_eq!(region, "bar");
                assert_eq!(&bucket, "baz");
                assert_eq!(aws_access_key_id.unwrap(), "abcd");
                assert_eq!(aws_secret_access_key.unwrap(), "1234");
                assert!(!aws_instance_profile);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn test_connectivity_s3_missing_params() {
        neg("zfs_object_agent test-connectivity-s3");
        neg("zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz");
        neg("zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz -i abcd");
        neg("zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz -s 1234");
    }

    #[test]
    fn test_connectivity_s3_param_conflicts() {
        neg("zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz -i abcd -s 1234 --aws-instance-profile");
        neg("zfs_object_agent test-connectivity-s3 -e foo -r bar -b baz -i abcd --aws-instance-profile");
    }

    #[test]
    fn test_connectivity_blob_missing_params() {
        neg("zfs_object_agent test-connectivity-blob");
        neg("zfs_object_agent test-connectivity-blob -b baz");
        neg("zfs_object_agent test-connectivity-blob -e foo -b baz -a abcd");
        neg("zfs_object_agent test-connectivity-blob -e foo -b baz -s abcd");
        neg("zfs_object_agent test-connectivity-blob -e foo -b baz --managed_identity");
    }

    #[test]
    fn test_connectivity_blob_param_conflicts() {
        neg("zfs_object_agent test-connectivity-blob -e foo -b baz -a abcd -s 1234 --managed_identity");
    }

    #[test]
    fn log_conflict() {
        neg("zfs_object_agent -l -v");
        neg("zfs_object_agent -l --output-file foo");
    }

    #[test]
    fn test_args_neg() {
        neg("zfs_object_agent -c disk1 -d /dev/");
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
