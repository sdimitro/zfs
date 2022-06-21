// This file is not used in production.
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use git_version::git_version;
use uuid::Uuid;
use zettaobject::object_access::BlobCredentials;
use zettaobject::object_access::ObjectAccess;
use zettaobject::object_access::ObjectAccessProtocol;
use zettaobject::object_access::S3Credentials;
mod perf;

const ENDPOINT: &str = "https://s3-us-west-2.amazonaws.com";
const REGION: &str = "us-west-2";
const BUCKET_NAME: &str = "cloudburst-data-2";

static GIT_VERSION: &str = git_version!(
    fallback = match option_env!("CARGO_ZOA_GITREV") {
        Some(value) => value,
        None => "unknown",
    }
);

#[derive(Args)]
struct S3Args {
    /// S3 endpoint
    #[clap(short = 'e', long, default_value = ENDPOINT)]
    endpoint: String,

    /// S3 region
    #[clap(short = 'r', long, default_value = REGION)]
    region: String,

    /// S3 bucket
    #[clap(short = 'b', long, default_value = BUCKET_NAME)]
    bucket: String,

    /// credentials profile
    #[clap(short = 'p', long)]
    profile: Option<String>,
}

#[derive(Args)]
struct BlobArgs {
    /// Blob endpoint
    #[clap(short = 'e', long)]
    endpoint: Option<String>,

    /// Blob bucket
    #[clap(short = 'b', long, default_value = BUCKET_NAME)]
    bucket: String,

    /// credentials profile
    #[clap(short = 'p', long)]
    profile: Option<String>,
}

#[derive(Parser)]
#[clap(version=GIT_VERSION)]
#[clap(name = "zfs_object_perf")]
#[clap(about = "ZFS object storage performance tests")]
#[clap(propagate_version = true)]
struct Cli {
    /// Object size in KiB
    #[clap(short = 's', long, default_value = "2048")]
    object_size: u64,

    /// number of concurrent GET/PUT operations
    #[clap(short = 'q', long, default_value = "10")]
    qdepth: u64,

    /// How long to run the test (in seconds)
    #[clap(short = 'd', long, default_value = "30", value_name = "SECONDS")]
    time: u64,

    /// Sets the level of logging verbosity
    #[clap(short = 'v', parse(from_occurrences))]
    verbosity: u64,

    /// File to log output to
    #[clap(short = 'o', long, value_name = "FILE")]
    output_file: Option<PathBuf>,

    /// Configuration file to set tunables (toml/json/yaml)
    #[clap(short = 't', long, value_name = "FILE")]
    config_file: Option<PathBuf>,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Write s3 test
    #[clap(alias = "write")]
    WriteS3 {
        #[clap(flatten)]
        s3_args: S3Args,
    },

    /// Read s3 test
    #[clap(alias = "read")]
    ReadS3 {
        #[clap(flatten)]
        s3_args: S3Args,
    },

    /// Write blob test
    WriteBlob {
        #[clap(flatten)]
        blob_args: BlobArgs,
    },

    /// Read blob test
    ReadBlob {
        #[clap(flatten)]
        blob_args: BlobArgs,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if let Some(file_name) = cli.config_file {
        if let Err(error) = util::tunable::read_config(&file_name) {
            println!("error: reading config: {}", error);
            std::process::exit(1);
        }
    }

    util::setup_logging(cli.verbosity, cli.output_file.as_deref(), None, true);

    let duration = Duration::from_secs(cli.time);
    let objsize_bytes = cli.object_size * 1024;

    let key_prefix = format!("zfs_object_perf/{}/", Uuid::new_v4());
    println!("Using prefix: '{}'", key_prefix);
    match cli.command {
        Commands::WriteS3 { s3_args } => {
            let object_access = ObjectAccess::new(
                ObjectAccessProtocol::S3 {
                    endpoint: s3_args.endpoint,
                    region: s3_args.region,
                    credentials: match s3_args.profile {
                        Some(profile) => S3Credentials::Profile(profile),
                        None => S3Credentials::Automatic,
                    },
                },
                s3_args.bucket,
                false,
            )
            .await
            .unwrap();

            perf::write_test(
                object_access,
                key_prefix,
                objsize_bytes,
                cli.qdepth,
                duration,
            )
            .await
            .unwrap();
        }
        Commands::ReadS3 { s3_args } => {
            let object_access = ObjectAccess::new(
                ObjectAccessProtocol::S3 {
                    endpoint: s3_args.endpoint,
                    region: s3_args.region,
                    credentials: match s3_args.profile {
                        Some(profile) => S3Credentials::Profile(profile),
                        None => S3Credentials::Automatic,
                    },
                },
                s3_args.bucket,
                false,
            )
            .await
            .unwrap();

            perf::read_test(
                object_access,
                key_prefix,
                objsize_bytes,
                cli.qdepth,
                duration,
            )
            .await
            .unwrap();
        }
        Commands::WriteBlob { blob_args } => {
            let object_access = ObjectAccess::new(
                ObjectAccessProtocol::Blob {
                    endpoint: blob_args.endpoint,
                    credentials: match blob_args.profile {
                        Some(profile) => BlobCredentials::Profile(profile),
                        None => BlobCredentials::Automatic,
                    },
                },
                blob_args.bucket,
                false,
            )
            .await
            .unwrap();

            perf::write_test(
                object_access,
                key_prefix,
                objsize_bytes,
                cli.qdepth,
                duration,
            )
            .await
            .unwrap();
        }
        Commands::ReadBlob { blob_args } => {
            let object_access = ObjectAccess::new(
                ObjectAccessProtocol::Blob {
                    endpoint: blob_args.endpoint,
                    credentials: match blob_args.profile {
                        Some(profile) => BlobCredentials::Profile(profile),
                        None => BlobCredentials::Automatic,
                    },
                },
                blob_args.bucket,
                false,
            )
            .await
            .unwrap();

            perf::read_test(
                object_access,
                key_prefix,
                objsize_bytes,
                cli.qdepth,
                duration,
            )
            .await
            .unwrap();
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
