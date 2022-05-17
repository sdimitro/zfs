// This file is not used in production.
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use clap::Subcommand;
use git_version::git_version;
use uuid::Uuid;
use zettaobject::object_access::ObjectAccess;
use zettaobject::object_access::ObjectAccessProtocol;
use zettaobject::object_access::S3Credentials;
mod s3perf;

const ENDPOINT: &str = "https://s3-us-west-2.amazonaws.com";
const REGION: &str = "us-west-2";
const BUCKET_NAME: &str = "cloudburst-data-2";

static GIT_VERSION: &str = git_version!(
    fallback = match option_env!("CARGO_ZOA_GITREV") {
        Some(value) => value,
        None => "unknown",
    }
);

#[derive(Parser)]
#[clap(version=GIT_VERSION)]
#[clap(name = "zfs_object_perf")]
#[clap(about = "ZFS object storage performance tests")]
#[clap(propagate_version = true)]
struct Cli {
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
    #[clap(short = 'p', long, default_value = "default")]
    profile: String,

    /// Object size in KiB
    #[clap(short = 's', long, default_value = "1024")]
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
    #[clap(
        short = 'o',
        long,
        value_name = "FILE",
        default_value = "/var/log/perflog"
    )]
    output_file: PathBuf,

    /// Configuration file to set tunables (toml/json/yaml)
    #[clap(short = 't', long, value_name = "FILE")]
    config_file: Option<PathBuf>,

    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// write test
    Write,
    /// read test
    Read,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    util::setup_logging(
        cli.verbosity,
        Some(&cli.output_file),
        cli.config_file.as_deref(),
        false,
    );

    let duration = Duration::from_secs(cli.time);
    let objsize_bytes = cli.object_size * 1024;

    println!(
        "endpoint: {}, region: {}, bucket: {} profile: {}",
        cli.endpoint, cli.region, cli.bucket, cli.profile
    );

    let object_access = ObjectAccess::new(
        ObjectAccessProtocol::S3 {
            endpoint: cli.endpoint,
            region: cli.region,
            credentials: S3Credentials::Profile(cli.profile.to_owned()),
        },
        cli.bucket,
        false,
    )
    .await
    .unwrap();

    let key_prefix = format!("zfs_object_perf/{}/", Uuid::new_v4());
    println!("Using prefix: '{}'", key_prefix);
    match cli.command {
        Commands::Write => {
            s3perf::write_test(
                object_access,
                key_prefix,
                objsize_bytes,
                cli.qdepth,
                duration,
            )
            .await
            .unwrap();
        }
        Commands::Read => {
            s3perf::read_test(
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
