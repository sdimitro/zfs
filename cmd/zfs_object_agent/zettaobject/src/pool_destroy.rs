use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::anyhow;
use anyhow::Result;
use futures::stream::StreamExt;
use lazy_static::lazy_static;
use log::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use util::maybe_die_with;
use util::tunable;
use zettacache::base_types::*;

use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessProtocol;
use crate::object_access::OBJECT_DELETION_BATCH_SIZE;
use crate::pool::PoolPhys;

lazy_static! {
    static ref POOL_DESTROYER: Mutex<Option<PoolDestroyer>> = Default::default();
}

tunable! {
    // Persist zpool destroy progress after this number of iterations of bulk object destroy.
    static ref DESTROY_PROGRESS_FREQUENCY: usize = 10;
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct PoolDestroyingPhys {
    start_time: SystemTime,
    total_data_objects: u64,
    destroyed_objects: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
enum PoolDestroyState {
    InProgress,
    Complete,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct DestroyingCacheItemPhys {
    name: String,
    #[serde(flatten)]
    protocol: ObjectAccessProtocol,
    bucket: String,
    state: PoolDestroyState,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct DestroyingCachePhys {
    pools: HashMap<PoolGuid, DestroyingCacheItemPhys>,
}

#[derive(Serialize, Debug, Clone)]
pub struct DestroyingPool {
    #[serde(flatten)]
    cache_phys: DestroyingCacheItemPhys,
    #[serde(flatten)]
    destroying_phys: Option<PoolDestroyingPhys>,
}

#[derive(Default, Debug)]
struct DestroyingPoolsMap {
    pools: HashMap<PoolGuid, DestroyingPool>,
}

impl DestroyingPoolsMap {
    fn from_phys(destroying_cache_phys: DestroyingCachePhys) -> Self {
        let pools = destroying_cache_phys
            .pools
            .iter()
            .map(|(guid, phys)| {
                (
                    *guid,
                    DestroyingPool {
                        cache_phys: phys.clone(),
                        destroying_phys: None,
                    },
                )
            })
            .collect::<HashMap<PoolGuid, DestroyingPool>>();

        DestroyingPoolsMap { pools }
    }

    fn to_phys(&self) -> DestroyingCachePhys {
        DestroyingCachePhys {
            pools: self
                .pools
                .iter()
                .map(|(guid, item)| (*guid, item.cache_phys.clone()))
                .collect(),
        }
    }

    fn increment_destroyed_objects(&mut self, guid: PoolGuid, delta: u64) {
        let destroying_pool = self.pools.get_mut(&guid).unwrap();
        let destroying_phys = destroying_pool.destroying_phys.as_mut().unwrap();
        destroying_phys.destroyed_objects += delta;

        trace!(
            "Updating zpool destroy list: {:?} total objects={} destroyed_objects={}.",
            guid,
            destroying_phys.total_data_objects,
            destroying_phys.destroyed_objects
        );
    }

    fn update_destroy_complete(&mut self, guid: PoolGuid) {
        let destroying_pool = self.pools.get_mut(&guid).unwrap();
        destroying_pool.cache_phys.state = PoolDestroyState::Complete;

        debug!(
            "update_destroy_complete: {:?} {:?}.",
            guid, destroying_pool.destroying_phys
        );
    }

    async fn remove_not_in_progress(&mut self) {
        debug!("Clearing destroyed pools.");
        {
            self.pools.retain(|_, destroying_pool| {
                destroying_pool.cache_phys.state == PoolDestroyState::InProgress
            });
        }
    }
}

#[derive(Default, Debug)]
struct PoolDestroyer {
    destroying_pools_map: DestroyingPoolsMap,

    // Full path to the zpool-destroy cache file, initialized at startup.
    destroy_cache_filename: PathBuf,
}

impl PoolDestroyer {
    async fn init(&mut self) -> Result<()> {
        match OpenOptions::new()
            .read(true)
            .open(&self.destroy_cache_filename)
            .await
        {
            Ok(mut cache_file) => {
                let mut buffer = String::new();
                cache_file.read_to_string(&mut buffer).await?;
                if !buffer.is_empty() {
                    self.destroying_pools_map =
                        DestroyingPoolsMap::from_phys(serde_json::from_str(&buffer)?);
                }

                // Fire off destroy tasks
                let destroying_pool_list =
                    self.destroying_pools_map
                        .pools
                        .iter()
                        .filter(|(_, destroying_pool)| {
                            destroying_pool.cache_phys.state == PoolDestroyState::InProgress
                        });

                for (guid, destroying_pool) in destroying_pool_list {
                    match ObjectAccess::new(
                        destroying_pool.cache_phys.protocol.clone(),
                        destroying_pool.cache_phys.bucket.clone(),
                        false,
                    )
                    .await
                    {
                        Ok(object_access) => {
                            start_destroy_task(object_access, *guid);
                        }
                        Err(e) => {
                            // Error likely caused by invalid credentials. Since
                            // there may be other pools that can be accesssed,
                            // log an error and keep going.
                            error!("Failed to connect to pool: {} {}", &guid, e);
                        }
                    };
                }

                Ok(())
            }
            Err(ref error) if error.kind() == ErrorKind::NotFound => {
                // zpool_destroy.cache file does not exist. No initialization is needed.
                info!("{:?} does not exist", &self.destroy_cache_filename);
                Ok(())
            }
            Err(error) => Err(anyhow!(
                "Error opening {:?}; {:?}",
                &self.destroy_cache_filename,
                error
            )),
        }
    }

    /// Write out the DestroyingPoolsMap as json so that if the agent restarts, it can continue
    /// destroying the pools that were in the process of being destroyed. To ensure that an agent
    /// crash does not leave the cache file partially written, we always write to a temp file and
    /// rename it atomically to replace the cache file.
    async fn write(&self) -> Result<()> {
        trace!("Writing out destroy cache file.");

        let temp_filename = format!(
            "{}.{}",
            self.destroy_cache_filename.to_string_lossy(),
            process::id()
        );

        fs::write(
            &temp_filename,
            serde_json::to_string_pretty(&self.destroying_pools_map.to_phys())
                .unwrap()
                .as_bytes(),
        )
        .await?;

        fs::rename(&temp_filename, &self.destroy_cache_filename).await?;

        Ok(())
    }

    /// Mark a pool as destroyed by writing to its super object.
    async fn mark_pool_destroying(
        &mut self,
        object_access: &ObjectAccess,
        guid: PoolGuid,
        total_data_objects: u64,
    ) {
        // Get super object and mark the pool as destroyed.
        let mut pool_phys = PoolPhys::get(object_access, guid).await.unwrap();
        assert!(pool_phys.destroying_state.is_none());

        // write super marking the pool as destroyed.
        pool_phys.destroying_state = Some(PoolDestroyingPhys {
            start_time: SystemTime::now(),
            total_data_objects,
            destroyed_objects: 0,
        });
        pool_phys.put(object_access).await;
    }

    /// Write to zpool_destroy.cache file so that we can resume destroy if the agent restarts.
    async fn add_zpool_destroy_cache(
        &mut self,
        object_access: &ObjectAccess,
        guid: PoolGuid,
    ) -> Result<()> {
        let pool_phys = PoolPhys::get(object_access, guid).await?;
        let destroyed_pool = pool_phys.destroying_state.unwrap();

        let destroying_pool = DestroyingPool {
            cache_phys: DestroyingCacheItemPhys {
                name: pool_phys.name,
                protocol: object_access.protocol(),
                bucket: object_access.bucket(),
                state: PoolDestroyState::InProgress,
            },
            destroying_phys: Some(PoolDestroyingPhys {
                start_time: destroyed_pool.start_time,
                total_data_objects: destroyed_pool.total_data_objects,
                destroyed_objects: destroyed_pool.destroyed_objects,
            }),
        };

        info!("marking pool {:?} destroyed; {:?}", guid, destroyed_pool);

        if self.destroying_pools_map.pools.contains_key(&guid) {
            return Err(anyhow!("pool {:?} already in destroying_pools_map", guid));
        }
        self.destroying_pools_map
            .pools
            .insert(guid, destroying_pool);

        self.write().await?;

        Ok(())
    }
}

fn delete_pool_objects(
    object_access: Arc<ObjectAccess>,
    guid: PoolGuid,
    sender: mpsc::UnboundedSender<usize>,
) {
    tokio::spawn(async move {
        let prefix = format!("zfs/{}/", guid);
        let super_object = PoolPhys::key(guid);
        let batch_size = *OBJECT_DELETION_BATCH_SIZE * *DESTROY_PROGRESS_FREQUENCY;
        let mut count = 0;

        object_access
            .delete_objects(
                object_access
                    .list_objects(prefix, false)
                    // Skip the super object as we use it to track the progress made by this task.
                    .filter(|o| futures::future::ready(super_object.ne(o)))
                    .inspect(|_| {
                        // Note: We are counting the objects listed rather than the objects deleted.
                        // This works as we are fine with not being very accurate.
                        count += 1;
                        if count >= batch_size {
                            sender.send(count).unwrap();
                            count = 0;
                        }
                    }),
            )
            .await;
    });
}

async fn destroy_task(object_access: Arc<ObjectAccess>, guid: PoolGuid) {
    info!("destroying pool {:?}", guid);

    // There can only be one destroy task for any given pool.
    // It is possible that the pool has been destroyed
    match PoolPhys::get(&object_access, guid).await {
        Ok(mut pool_phys) => {
            POOL_DESTROYER
                .lock()
                .await
                .as_mut()
                .unwrap()
                .destroying_pools_map
                .pools
                .get_mut(&guid)
                .unwrap()
                .destroying_phys = pool_phys.destroying_state;

            let (tx, mut rx) = mpsc::unbounded_channel();
            delete_pool_objects(object_access.clone(), guid, tx);

            // Wait for the delete_prefix task to send progress updates.
            while let Some(object_count) = rx.recv().await {
                POOL_DESTROYER
                    .lock()
                    .await
                    .as_mut()
                    .unwrap()
                    .destroying_pools_map
                    .increment_destroyed_objects(guid, object_count as u64);

                // Update PoolPhys
                pool_phys
                    .destroying_state
                    .as_mut()
                    .unwrap()
                    .destroyed_objects += object_count as u64;
                pool_phys.put(&object_access).await;
            }

            // The super object is destroyed last as it is used to keep track of the progress made.
            object_access.delete_object(PoolPhys::key(guid)).await;
        }
        Err(err) => {
            info!("pool {:?} already destroyed, {:?}", guid, err);
        }
    }
    let mut maybe_pool_destroyer = POOL_DESTROYER.lock().await;
    let pool_destroyer = maybe_pool_destroyer.as_mut().unwrap();
    pool_destroyer
        .destroying_pools_map
        .update_destroy_complete(guid);
    pool_destroyer.write().await.unwrap();
}

fn start_destroy_task(object_access: Arc<ObjectAccess>, guid: PoolGuid) {
    tokio::spawn(async move {
        destroy_task(object_access, guid).await;
    });
}

/// Mark a pool as destroyed and start destroying it in the background.
pub async fn destroy_pool(
    object_access: Arc<ObjectAccess>,
    guid: PoolGuid,
    total_data_objects: u64,
) {
    let mut maybe_pool_destroyer = POOL_DESTROYER.lock().await;
    let pool_destroyer = maybe_pool_destroyer.as_mut().unwrap();
    pool_destroyer
        .mark_pool_destroying(&object_access, guid, total_data_objects)
        .await;
    pool_destroyer
        .add_zpool_destroy_cache(&object_access, guid)
        .await
        .unwrap();
    start_destroy_task(object_access, guid);

    maybe_die_with(|| "after destroy_pool");
}

/// Resume destroying a pool that was previously marked for destroying.
pub async fn resume_destroy(object_access: Arc<ObjectAccess>, guid: PoolGuid) -> Result<()> {
    // Fail the request if resumption of deletion is being requested on a pool that is not in
    // destroyed state.
    let mut maybe_pool_destroyer = POOL_DESTROYER.lock().await;
    let pool_destroyer = maybe_pool_destroyer.as_mut().unwrap();
    match PoolPhys::get(&object_access, guid).await {
        Ok(pool_phys) => match pool_phys.destroying_state {
            Some(_) => {
                pool_destroyer
                    .add_zpool_destroy_cache(&object_access, guid)
                    .await?;
                start_destroy_task(object_access, guid);
                maybe_die_with(|| "in resume_destroy");

                Ok(())
            }
            None => Err(anyhow!("pool {:?} not in destroyed state", guid)),
        },
        Err(error) => Err(anyhow!("pool {:?} not found, {:?}", guid, error)),
    }
}

/// Retrieve the PoolDestroyer's list of pools that are either being destroyed or have been
/// destroyed.
pub async fn get_destroy_list() -> HashMap<PoolGuid, DestroyingPool> {
    maybe_die_with(|| "in get_destroy_list");

    let maybe_pool_destroyer = POOL_DESTROYER.lock().await;
    maybe_pool_destroyer
        .as_ref()
        .unwrap()
        .destroying_pools_map
        .pools
        .clone()
}

/// Remove pools that have been successfully destroyed from the PoolDestroyer's list of pools.
pub async fn remove_not_in_progress() {
    maybe_die_with(|| "in remove_not_in_progress");

    let mut maybe_pool_destroyer = POOL_DESTROYER.lock().await;
    let pool_destroyer = maybe_pool_destroyer.as_mut().unwrap();
    pool_destroyer
        .destroying_pools_map
        .remove_not_in_progress()
        .await;
    pool_destroyer.write().await.unwrap();
}

pub async fn init_pool_destroyer(socket_dir: &Path) {
    // The PoolDestroyer should be initialized only once.
    let mut maybe_pool_destroyer = POOL_DESTROYER.lock().await;
    assert!(maybe_pool_destroyer.is_none());

    // Filename for zpool-destroy cache file.
    let mut destroyer = PoolDestroyer {
        destroying_pools_map: Default::default(),
        destroy_cache_filename: socket_dir.join("zpool_destroy.cache"),
    };
    destroyer.init().await.unwrap();

    *maybe_pool_destroyer = Some(destroyer);
}
