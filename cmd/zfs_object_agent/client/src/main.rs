// This file is not used in production.
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::fs::File;
use std::io::prelude::*;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use ::util::writeln_stderr;
use ::util::writeln_stdout;
use chrono::prelude::*;
use chrono::DateTime;
use clap::Parser;
use clap::Subcommand;
use client::Client;
use futures::stream::StreamExt;
use git_version::git_version;
use nvpair::NvEncoding;
use nvpair::NvList;
use rand::prelude::*;
use rusoto_core::ByteStream;
use rusoto_credential::ChainProvider;
use rusoto_credential::DefaultCredentialsProvider;
use rusoto_credential::InstanceMetadataProvider;
use rusoto_credential::ProfileProvider;
use rusoto_credential::ProvideAwsCredentials;
use rusoto_s3::*;
use tokio::io::AsyncReadExt;
use zettacache::base_types::*;
use zettaobject::access_stats::ObjectAccessOpType;
use zettaobject::base_types::*;
use zettaobject::data_object::DataObject;
use zettaobject::object_access::BlobCredentials;
use zettaobject::object_access::BucketAccess;
use zettaobject::Pool;
mod client;
use ::util::setup_logging;
use itertools::Itertools;
use zettaobject::object_access::ObjectAccess;
use zettaobject::object_access::ObjectAccessProtocol;
use zettaobject::object_access::S3Credentials;

const ENDPOINT: &str = "https://s3-us-west-2.amazonaws.com";
const REGION: &str = "us-west-2";
const BUCKET_NAME: &str = "cloudburst-data-2";
const POOL_NAME: &str = "testpool";
const POOL_GUID: u64 = 1234;

static GIT_VERSION: &str = git_version!(
    fallback = match option_env!("CARGO_ZOA_GITREV") {
        Some(value) => value,
        None => "unknown",
    }
);

async fn do_rusoto_provider<P>(credentials_provider: P, file: &str)
where
    P: ProvideAwsCredentials + Send + Sync + 'static,
{
    let http_client = rusoto_core::HttpClient::new().unwrap();
    let client = S3Client::new_with(
        http_client,
        credentials_provider,
        rusoto_core::Region::UsWest2,
    );

    let content = "I want to go to S3".as_bytes().to_vec();

    println!("putting {}", file);
    let req = PutObjectRequest {
        bucket: BUCKET_NAME.to_string(),
        key: file.to_string(),
        body: Some(ByteStream::from(content)),
        ..Default::default()
    };
    match client.put_object(req).await {
        Ok(result) => {
            println!("Success {:?}", result);
        }
        Err(result) => {
            println!("Failure {:?}", result);
        }
    }
}

async fn do_rusoto_role() -> Result<(), Box<dyn Error>> {
    do_rusoto_provider(
        InstanceMetadataProvider::new(),
        "test/InstanceMetadataProvider.txt",
    )
    .await;

    do_rusoto_provider(ProfileProvider::new().unwrap(), "test/ProfileProvider.txt").await;
    do_rusoto_provider(ChainProvider::new(), "test/ChainProvider.txt").await;
    do_rusoto_provider(
        DefaultCredentialsProvider::new().unwrap(),
        "test/DefaultCredentialsProvider.txt",
    )
    .await;

    Ok(())
}

async fn do_s3_rusoto() -> Result<(), Box<dyn Error>> {
    let client = S3Client::new(rusoto_core::Region::UsWest2);

    let key = "mahrens/test.file2";

    println!("getting {}", key);
    let req = GetObjectRequest {
        bucket: BUCKET_NAME.to_string(),
        key: key.to_string(),
        ..Default::default()
    };
    let res = client.get_object(req).await?;
    let mut s = String::new();
    res.body
        .unwrap()
        .into_async_read()
        .read_to_string(&mut s)
        .await
        .unwrap();
    println!("object contents = {}", s);

    let content = "I want to go to S3".as_bytes().to_vec();
    println!("putting {}", key);
    let req = PutObjectRequest {
        bucket: BUCKET_NAME.to_string(),
        key: key.to_string(),
        body: Some(ByteStream::from(content)),
        ..Default::default()
    };
    client.put_object(req).await?;

    Ok(())
}

fn do_btree() {
    let mut bt: BTreeSet<[u64; 8]> = BTreeSet::new();
    let mut rng = rand::thread_rng();
    let n = 10000000;
    for _ in 0..n {
        bt.insert(rng.gen());
    }
    println!("added {} items to btree", n);
    std::thread::sleep(Duration::from_secs(1000));
}

async fn do_create() -> Result<(), Box<dyn Error>> {
    let mut client = Client::connect().await;
    let endpoint = ENDPOINT;
    let region = REGION;
    let bucket_name = BUCKET_NAME;
    let pool_name = POOL_NAME;
    let guid = PoolGuid(POOL_GUID);

    client
        .create_pool(region, endpoint, bucket_name, guid, pool_name)
        .await;
    client.get_next_response().await;

    Ok(())
}

async fn do_write() -> Result<(), Box<dyn Error>> {
    let guid = PoolGuid(1234);
    let (mut client, next_txg, mut next_block) = setup_client(guid).await;

    let begin = Instant::now();
    let n = 5200;

    client.begin_txg(guid, next_txg).await;

    let task = client.get_responses_initiate(n);

    for _ in 0..n {
        let mut data: Vec<u8> = Vec::new();
        let mut rng = thread_rng();
        let len = rng.gen::<u32>() % 100;
        for _ in 0..len {
            data.push(rng.gen());
        }
        //println!("requesting write of {}B", len);
        client.write_block(guid, next_block, &data).await;
        //println!("writing {}B to {:?}...", len, id);
        next_block = BlockId(next_block.0 + 1);
    }
    client.flush_writes(guid).await;
    client.get_responses_join(task).await;

    client.end_txg(guid, &[]).await;
    client.get_next_response().await;

    println!("wrote {} blocks in {}ms", n, begin.elapsed().as_millis());

    Ok(())
}

async fn setup_client(guid: PoolGuid) -> (Client, Txg, BlockId) {
    let mut client = Client::connect().await;

    let bucket_name = BUCKET_NAME;
    let endpoint = ENDPOINT;
    let region = REGION;

    client.open_pool(region, endpoint, bucket_name, guid).await;

    let nvl = client.get_next_response().await;
    let txg = Txg(nvl.lookup_uint64("next txg").unwrap());
    let block = BlockId(nvl.lookup_uint64("next block").unwrap());

    (client, txg, block)
}

async fn do_read() -> Result<(), Box<dyn Error>> {
    let guid = PoolGuid(1234);
    let (mut client, _, _) = setup_client(guid).await;

    let max = 1000;
    let begin = Instant::now();
    let n = 50;

    let task = client.get_responses_initiate(n);

    for _ in 0..n {
        let id = BlockId((thread_rng().gen::<u64>() + 1) % max);
        client.read_block(guid, id).await;
    }

    client.get_responses_join(task).await;

    println!("read {} blocks in {}ms", n, begin.elapsed().as_millis());

    Ok(())
}

async fn do_free() -> Result<(), Box<dyn Error>> {
    let guid = PoolGuid(1234);
    let (mut client, mut next_txg, mut next_block) = setup_client(guid).await;

    // write some blocks, which we will then free some of

    client.begin_txg(guid, next_txg).await;
    next_txg = Txg(next_txg.0 + 1);

    let num_writes: usize = 10000;
    let task = client.get_responses_initiate(num_writes);
    let mut ids = Vec::new();

    for _ in 0..num_writes {
        let mut data: Vec<u8> = Vec::new();
        let mut rng = thread_rng();
        let len = rng.gen::<u32>() % 100;
        for _ in 0..len {
            data.push(rng.gen());
        }
        //println!("requesting write of {}B", len);
        client.write_block(guid, next_block, &data).await;
        //println!("writing {}B to {:?}...", len, id);
        ids.push(next_block);
        next_block = BlockId(next_block.0 + 1);
    }
    client.flush_writes(guid).await;
    client.get_responses_join(task).await;

    client.end_txg(guid, &[]).await;
    client.get_next_response().await;

    // free half the blocks, randomly selected
    client.begin_txg(guid, next_txg).await;

    for i in rand::seq::index::sample(&mut thread_rng(), ids.len(), ids.len() / 2) {
        client.free_block(guid, ids[i]).await;
    }
    client.end_txg(guid, &[]).await;
    client.get_next_response().await;

    Ok(())
}

fn get_file_as_byte_vec(filename: &str) -> Vec<u8> {
    let mut f = File::open(filename).expect("no file found");
    let metadata = fs::metadata(filename).expect("unable to read metadata");
    let mut buffer = vec![0; metadata.len() as usize];
    f.read_exact(&mut buffer).expect("buffer overflow");

    buffer
}

fn write_file_as_bytes(filename: &str, contents: &[u8]) {
    let mut f = File::create(filename).unwrap();
    f.write_all(contents).unwrap();
}

fn do_nvpair() {
    let buf = get_file_as_byte_vec("/etc/zfs/zpool.cache");
    let nvp = &mut NvList::try_unpack(buf.as_slice()).unwrap();
    //let nvp = &mut NvListRef::unpack(&buf[..]).unwrap();

    nvp.insert("new int", &5).unwrap();

    let vec: Vec<u8> = vec![1, 2, 3];
    nvp.insert("new uint8 array", vec.as_slice()).unwrap();

    println!("{:#?}", nvp);

    let newbuf = nvp.pack(NvEncoding::Native).unwrap();
    write_file_as_bytes("./zpool.cache.rust", &newbuf);
}

fn has_expired(mod_time: &DateTime<FixedOffset>, min_age: Duration) -> bool {
    let age = Local::now().signed_duration_since(*mod_time);
    min_age == Duration::from_secs(0) || age > chrono::Duration::from_std(min_age).unwrap()
}

async fn print_super(
    object_access: &ObjectAccess,
    pool_key: &str,
    mod_time: &DateTime<FixedOffset>,
) {
    print!("{:30} {:40}", mod_time, pool_key);
    let split: Vec<&str> = pool_key.rsplitn(3, '/').collect();
    let guid_str: &str = split[1];
    if let Ok(guid64) = str::parse::<u64>(guid_str) {
        let guid = PoolGuid(guid64);
        match Pool::get_config(object_access, guid).await {
            Ok(pool_config) => {
                let name = pool_config.lookup_string("name").unwrap();
                let hostname = pool_config.lookup_string("hostname").unwrap();
                println!(
                    "\t{:20} {}",
                    name.to_str().unwrap(),
                    hostname.to_str().unwrap()
                );
            }
            Err(_e) => {
                println!("\t-unknown format-");
            }
        }
    }
}

async fn find_old_pools(object_access: &ObjectAccess, min_age: Duration) -> Vec<String> {
    let pool_keys: Vec<String> = object_access
        .list_prefixes("zfs/".to_string())
        .collect()
        .await;
    let mut vec = Vec::new();
    for pool_key in pool_keys {
        match object_access
            .stat_object(format!("{}super", pool_key))
            .await
        {
            Some(output) => {
                let mod_time = output.last_modified.unwrap();
                print_super(object_access, &pool_key, &mod_time).await;
                if has_expired(&mod_time, min_age) {
                    vec.push(pool_key);
                } else {
                    println!(
                        "Skipping pool as it is not {} days old.",
                        min_age.as_secs() / (60 * 60 * 24)
                    );
                }
            }
            None => {
                println!("didn't find super object for {}", pool_key);
            }
        }
    }
    vec
}

async fn do_list_pools(
    object_access: &ObjectAccess,
    list_all_objects: bool,
) -> Result<(), Box<dyn Error>> {
    for pool_key in find_old_pools(object_access, Duration::from_secs(0)).await {
        // Lookup all objects in the pool.
        if list_all_objects {
            object_access
                .list_objects(pool_key, None, false)
                .for_each(|object| async move { println!("    {}", object) })
                .await;
        }
    }
    Ok(())
}

async fn do_destroy_old_pools(
    object_access: &ObjectAccess,
    min_age: Duration,
) -> Result<(), Box<dyn Error>> {
    for pool_keys in find_old_pools(object_access, min_age).await {
        object_access
            .delete_objects(object_access.list_objects(pool_keys, None, false))
            .await;
    }
    Ok(())
}

async fn do_dump_object(
    object_access: Arc<ObjectAccess>,
    pool_guid: u64,
    object: u64,
    verbose: usize,
) {
    match DataObject::get_uncached(
        &object_access,
        PoolGuid(pool_guid),
        ObjectId::new(BlockId(object)),
        ObjectAccessOpType::ReadsGet,
    )
    .await
    {
        Ok(obj) => {
            writeln_stdout!("Data object header: {}", obj);
            for (k, v) in obj.blocks.iter().sorted() {
                if verbose < 2 {
                    writeln_stdout!(
                        "Block id: {}, Length: {}, Contents: {:?}...",
                        k,
                        v.len(),
                        v.slice(0..8)
                    );
                } else {
                    writeln_stdout!("Block id: {}, Length: {}, Contents: {:?}", k, v.len(), v);
                }
            }
        }
        Err(e) => {
            writeln_stderr!("Failed to access DataObject: {:?}", e);
        }
    };
}

async fn get_object_access(
    endpoint: &str,
    region: &str,
    bucket: &str,
    profile: &str,
    aws_access_key_id: Option<&str>,
    aws_secret_access_key: Option<&str>,
) -> Arc<ObjectAccess> {
    let credentials = match aws_access_key_id {
        None => S3Credentials::Profile(profile.to_string()),
        Some(aws_access_key_id) =>
        // If access_id is specified, aws_secret_access_key should also be specified.
        {
            S3Credentials::Key {
                aws_access_key_id: aws_access_key_id.to_string(),
                aws_secret_access_key: aws_secret_access_key.unwrap().to_string(),
            }
        }
    };
    ObjectAccess::new(
        ObjectAccessProtocol::S3 {
            endpoint: endpoint.to_string(),
            region: region.to_string(),
            credentials,
        },
        bucket.to_string(),
        false,
    )
    .await
    .unwrap()
}

async fn do_blob(bucket: String, profile: String) -> Result<(), Box<dyn Error>> {
    let key = "blob2.txt".to_string();
    let bucket_access = BucketAccess::new(ObjectAccessProtocol::Blob {
        credentials: BlobCredentials::Profile(profile.clone()),
    })
    .await?;
    let buckets = bucket_access.list_buckets().await;
    println!("List containers {:?}", buckets);
    if !buckets.contains(&bucket) {
        return Err(format!("Bucket {} not found.", bucket).into());
    }

    let object_access = ObjectAccess::new(
        ObjectAccessProtocol::Blob {
            credentials: BlobCredentials::Profile(profile.clone()),
        },
        bucket,
        false,
    )
    .await?;

    let content = "I want to go to azure".as_bytes().to_vec();
    object_access
        .put_object_stream(
            key.clone(),
            || (ByteStream::from(content.clone()), content.len()),
            ObjectAccessOpType::MetadataPut,
        )
        .await;

    let bytes = object_access
        .get_object(key.clone(), ObjectAccessOpType::ReadsGet)
        .await?;
    println!("Get blob data [{}]: {:?}", key, bytes);

    let stat = object_access.stat_object(key.clone()).await;
    println!("Last modified: [{}]: {:?}", key, stat);

    println!(
        "List blobs {:?}",
        object_access
            .list_objects("".to_string(), None, true)
            .collect::<Vec<String>>()
            .await
    );

    object_access.delete_object(key).await;

    Ok(())
}

#[derive(Parser)]
//#[clap(long_about = None)]
#[clap(version=GIT_VERSION)]
#[clap(about = "ZFS Object Agent test")]
#[clap(propagate_version = true)]
struct Cli {
    #[clap(short, long, help = "S3 endpoint", default_value = ENDPOINT)]
    endpoint: String,
    #[clap(short, long, help = "S3 region", default_value = REGION)]
    region: String,
    #[clap(short, long, help = "S3 bucket", default_value = BUCKET_NAME)]
    bucket: String,
    #[clap(short, long, help = "credentials profile", default_value = "default")]
    profile: String,
    #[clap(short = 'i', long, requires = "aws-secret-access-key")]
    aws_access_key_id: Option<String>,
    #[clap(short = 's', long, requires = "aws-access-key-id")]
    aws_secret_access_key: Option<String>,
    #[clap(short, long, parse(from_occurrences))]
    verbose: usize,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    S3Rusoto,
    Blob,
    Create,
    Write,
    Read,
    Free,
    Btree,
    Nvpair,
    RusotoRole,
    ListPools,
    ListPoolObjects,
    DestroyOldPools {
        #[clap(short = 'd', long)]
        days: u64,
    },
    DumpObject {
        #[clap(short, long)]
        pool_guid: u64,
        #[clap(short, long)]
        object: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    if cli.verbose > 0 {
        println!(
            "endpoint: {}, region: {}, bucket: {} profile: {} access_id: {:?}, secret_key: {:?}",
            cli.endpoint,
            cli.region,
            cli.bucket,
            cli.profile,
            cli.aws_access_key_id,
            cli.aws_secret_access_key
        );
    }

    let object_access = get_object_access(
        &cli.endpoint,
        &cli.region,
        &cli.bucket,
        &cli.profile,
        cli.aws_access_key_id.as_deref(),
        cli.aws_secret_access_key.as_deref(),
    )
    .await;

    setup_logging(cli.verbose as u64, None, None, false);

    match cli.command {
        Commands::S3Rusoto => do_s3_rusoto().await?,
        Commands::Blob => do_blob(cli.bucket, cli.profile).await?,
        Commands::Create => do_create().await?,
        Commands::Write => do_write().await?,
        Commands::Read => do_read().await?,
        Commands::Free => do_free().await?,
        Commands::Btree => do_btree(),
        Commands::Nvpair => do_nvpair(),
        Commands::RusotoRole => do_rusoto_role().await?,
        Commands::ListPools => do_list_pools(&object_access, false).await?,
        Commands::ListPoolObjects => do_list_pools(&object_access, true).await?,
        Commands::DestroyOldPools { days } => {
            let min_age = Duration::from_secs(days * 60 * 60 * 24);
            do_destroy_old_pools(&object_access, min_age).await?;
        }
        Commands::DumpObject { pool_guid, object } => {
            do_dump_object(object_access, pool_guid, object, cli.verbose).await
        }
    }
    Ok(())
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
