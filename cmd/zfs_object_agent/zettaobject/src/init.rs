use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::mem;
use std::path::Path;
use std::process;
use std::sync::Arc;

use fs2::FileExt;
use log::*;
use tokio::runtime::Runtime;
use util::register_siguser1_to_dump_tracing;
use uuid::Uuid;
use zettacache::CacheOpenError;
use zettacache::CacheOpenMode;
use zettacache::ZettaCache;

use crate::pool_destroy;
use crate::public_connection::PublicServerState;
use crate::root_connection::RootServerState;

fn lock_socket_dir(socket_dir: &Path) {
    let lock_file = socket_dir.join("zoa.lock");
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_file)
    {
        Ok(mut file) => {
            match file.try_lock_exclusive() {
                Ok(_) => {
                    let pid = format!("{}", process::id());
                    file.set_len(0).unwrap();
                    file.write_all(pid.as_bytes()).unwrap();

                    /*
                     * The exclusive lock on the file is held until it is closed. Since we want
                     * to hold that lock until this process exits, we need to hold on to the
                     * file. But since we don't need to access the file anymore, we just "forget"
                     * about the file without running its destructor.
                     */
                    mem::forget(file);
                }
                Err(error) => {
                    let mut buffer = String::new();
                    file.read_to_string(&mut buffer).unwrap();
                    error!(
                        "Another zfs_object_agent process with pid {} is running. Error: {:?}",
                        buffer, error
                    );
                    std::process::exit(2);
                }
            }
        }
        Err(_) => {
            error!("Failed to create lock file {lock_file:?}");
            std::process::exit(1);
        }
    }
}

fn parse_id_from_file(id_path: &Path) -> Result<Uuid, anyhow::Error> {
    let mut f = File::open(id_path)?;

    let mut bytes = Vec::new();
    assert_eq!(f.read_to_end(&mut bytes)?, uuid::fmt::Hyphenated::LENGTH);
    Ok(Uuid::parse_str(std::str::from_utf8(&bytes)?)?)
}

pub fn start(
    socket_dir: &Path,
    cache_mode: CacheOpenMode,
    clear_incompatible_cache: bool,
    runtime: Runtime,
) -> Result<(), anyhow::Error> {
    register_siguser1_to_dump_tracing()?;

    /*
     * Take an exclusive lock on a lock file. This prevents multiple agent
     * processes from operating out of the same socket_dir.
     */
    lock_socket_dir(socket_dir);

    runtime.block_on(async move {
        // Kick off zpool destroy tasks.
        pool_destroy::init_pool_destroyer(socket_dir).await;

        let cache = match ZettaCache::open(cache_mode.clone()).await {
            Ok(cache) => Arc::new(cache),
            Err(CacheOpenError::IncompatibleFeatures(paths, e)) if clear_incompatible_cache => {
                warn!("Clearing incompatible cache: {e:?}");
                ZettaCache::create(paths).await?;
                Arc::new(ZettaCache::open(cache_mode).await?)
            }
            Err(e) => return Err(e.into()),
        };

        PublicServerState::start(socket_dir, cache.clone());

        let id_path = Path::new("/run/zfs_agent_id");

        let id = parse_id_from_file(id_path).unwrap_or_else(|err| {
            trace!("Opening agent id failed: {:?}", err);
            let mut file = File::create(id_path).unwrap();
            let uuid = Uuid::new_v4();
            let mut buf = [0; uuid::fmt::Hyphenated::LENGTH];
            uuid.hyphenated().encode_lower(&mut buf);
            file.write_all(&buf).unwrap();
            uuid
        });

        RootServerState::start(socket_dir, cache, id);

        // keep the process from exiting
        futures::future::pending::<()>().await;
        Ok(())
    })
}
