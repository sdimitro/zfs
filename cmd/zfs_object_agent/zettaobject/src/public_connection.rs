use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use futures::stream::StreamExt;
use log::*;
use nvpair::NvList;
use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use util::maybe_die_with;
use util::message::*;
use util::tunable;
use util::ReportHitsResponse;
use zettacache::base_types::*;
use zettacache::ZettaCache;

use crate::object_access::BucketAccess;
use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessProtocol;
use crate::pool::*;
use crate::pool_destroy;
use crate::pool_destroy::DestroyingPool;
use crate::server::return_ok;
use crate::server::return_result;
use crate::server::ConnectionState;
use crate::server::FailureMessage;
use crate::server::HandlerReturn;
use crate::server::Server;

tunable! {
    pub static ref GET_POOLS_QUEUE_DEPTH: usize = 100;
}

pub struct PublicServerState {
    cache: Arc<ZettaCache>,
}

struct PublicConnectionState {
    cache: Arc<ZettaCache>,
    version: Option<Version>,
}

impl ConnectionState for PublicConnectionState {
    fn set_version(&mut self, version: Version) {
        assert!(self.version.is_none());
        self.version = Some(version);
    }
}

impl PublicServerState {
    fn connection_handler(&self) -> PublicConnectionState {
        PublicConnectionState {
            cache: self.cache.clone(),
            version: None,
        }
    }

    pub fn start(socket_dir: &Path, cache: Arc<ZettaCache>) {
        let socket_path = socket_dir.join("zfs_public_socket");

        let mut server = Server::new(
            &socket_path,
            0o666, // world writable
            PublicServerState { cache },
            Box::new(Self::connection_handler),
            vec![Version::new(2, 0, 0)],
        );

        PublicConnectionState::register(&mut server);

        server.start();
    }
}

impl PublicConnectionState {
    fn register(server: &mut Server<PublicServerState, PublicConnectionState>) {
        server.register_handler(TYPE_GET_POOLS, Box::new(Self::get_pools));
        server.register_handler(
            TYPE_GET_DESTROYING_POOLS,
            Box::new(Self::get_destroying_pools),
        );
        server.register_handler(
            TYPE_CLEAR_DESTROYED_POOLS,
            Box::new(Self::clear_destroyed_pools),
        );
        server.register_handler(TYPE_REPORT_HITS, Box::new(Self::report_hits));
        server.register_handler(TYPE_LIST_DEVICES, Box::new(Self::list_devices));
        server.register_handler(TYPE_ZCACHE_IOSTAT, Box::new(Self::zcache_iostat));
        server.register_handler(TYPE_ZCACHE_STATS, Box::new(Self::zcache_stats));
    }

    fn get_pools(&mut self, nvl: NvList) -> HandlerReturn {
        info!("got request: {:?}", nvl);
        Ok(Box::pin(async move {
            #[derive(Debug, Deserialize)]
            struct GetPoolsRequest {
                #[serde(flatten)]
                protocol: ObjectAccessProtocol,
                bucket: Option<String>,
                guid: Option<u64>,
            }
            let request: GetPoolsRequest =
                match nvpair::from_nvlist(&nvl).context("bad or insufficient parameters") {
                    Ok(request) => request,
                    Err(e) => {
                        let result = Err::<(), _>(FailureMessage::new(e));
                        return return_result((), result, true);
                    }
                };
            let bucket_access = match BucketAccess::new(request.protocol.clone())
                .await
                .context("connection error")
            {
                Ok(bucket_access) => bucket_access,
                Err(e) => {
                    let result = Err::<(), _>(FailureMessage::new(e));
                    return return_result((), result, true);
                }
            };
            let buckets = if let Some(bucket) = request.bucket {
                vec![bucket]
            } else {
                bucket_access.list_buckets().await
            };

            maybe_die_with(|| "in get_pools_impl");

            let pools = Arc::new(Mutex::new(NvList::new_unique_names()));
            for buck in buckets {
                let object_access = ObjectAccess::new(request.protocol.clone(), buck, true).await?;
                if let Some(guid) = request.guid {
                    find_pool(object_access, PoolGuid(guid), pools.clone()).await;
                } else {
                    discover_pools(object_access, pools.clone()).await;
                }
            }
            let pools = Arc::try_unwrap(pools).unwrap().into_inner().unwrap();
            let mut response = NvList::new_unique_names();
            response.insert("pools", pools.as_ref())?;
            info!("sending response: {response:?}");
            Ok(Some(response))
        }))
    }

    fn get_destroying_pools(&mut self, nvl: NvList) -> HandlerReturn {
        Ok(Box::pin(async move {
            debug!("got request: {:?}", nvl);

            #[derive(Debug, Serialize)]
            struct DestroyingPoolsResponse {
                pools: HashMap<String, DestroyingPool>,
            }
            let response = DestroyingPoolsResponse {
                pools: pool_destroy::get_destroy_list()
                    .await
                    .into_iter()
                    .map(|(guid, destroying)| (guid.to_string(), destroying))
                    .collect(),
            };
            return_ok(response, true)
        }))
    }

    fn clear_destroyed_pools(&mut self, nvl: NvList) -> HandlerReturn {
        Ok(Box::pin(async move {
            // XXX convert to use serde nvlist response
            debug!("got request: {:?}", nvl);
            pool_destroy::remove_not_in_progress().await;

            Ok(Some(NvList::new_unique_names()))
        }))
    }

    fn report_hits(&mut self, _: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        debug!("got ReportHitsRequest");
        Ok(Box::pin(async move {
            let phys = cache.hits_by_size_data().await;
            let mut combined_histogram = Vec::new();
            let mut real_hits = 0;
            for (live_hits, ghost_hits) in phys.live_histogram.iter().zip(&phys.ghost_histogram) {
                real_hits += live_hits;
                combined_histogram.push(live_hits + ghost_hits);
            }

            let response = ReportHitsResponse {
                started: phys.started(),
                lookups: phys.lookups,
                real_hits,
                cache_capacity: phys.cache_capacity,
                bucket_size: phys.bucket_size,
                combined_histogram,
            };
            return_ok(response, true)
        }))
    }

    fn list_devices(&mut self, _: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        debug!("got ListDevicesRequest");
        Ok(Box::pin(async move {
            #[derive(Debug, Serialize)]
            struct ListDevicesResponse {
                devices_json: String,
            }
            let response = ListDevicesResponse {
                devices_json: serde_json::to_string(&cache.devices()).unwrap(),
            };
            return_ok(response, true)
        }))
    }

    fn zcache_iostat(&mut self, _: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        trace!("got ZcacheIostatRequest");
        Ok(Box::pin(async move {
            #[derive(Debug, Serialize)]
            struct ZcacheIostatResponse {
                iostats_json: String,
            }
            let response = ZcacheIostatResponse {
                iostats_json: serde_json::to_string(&cache.io_stats()).unwrap(),
            };
            return_ok(response, false)
        }))
    }

    fn zcache_stats(&mut self, _: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        trace!("got ZcacheStatsRequest");
        Ok(Box::pin(async move {
            #[derive(Debug, Serialize)]
            struct ZcacheStatsResponse {
                stats_json: String,
            }
            let response = ZcacheStatsResponse {
                stats_json: serde_json::to_string(&cache.stats()).unwrap(),
            };
            return_ok(response, false)
        }))
    }
}

async fn discover_pools(object_access: Arc<ObjectAccess>, pools: Arc<Mutex<NvList>>) {
    object_access
        .list_prefixes("zfs/".to_string())
        .for_each_concurrent(*GET_POOLS_QUEUE_DEPTH, |prefix| {
            let my_object_access = object_access.clone();
            let pools = pools.clone();
            async move {
                debug!("prefix: {}", prefix);
                let split = prefix.rsplitn(3, '/').collect::<Vec<_>>();
                let guid_str = split[1];
                if let Ok(guid64) = str::parse::<u64>(guid_str) {
                    let guid = PoolGuid(guid64);
                    match Pool::get_config(&my_object_access, guid).await {
                        Ok(pool_config) => pools
                            .lock()
                            .unwrap()
                            .insert(guid_str, pool_config.as_ref())
                            .unwrap(),
                        Err(e) => {
                            error!("skipping {guid:?}: {e:?}");
                        }
                    }
                }
            }
        })
        .await;
}

async fn find_pool(object_access: Arc<ObjectAccess>, guid: PoolGuid, pools: Arc<Mutex<NvList>>) {
    if Pool::exists(&object_access, guid).await {
        match Pool::get_config(&object_access, guid).await {
            Ok(pool_config) => {
                pools
                    .lock()
                    .unwrap()
                    .insert(format!("{}", guid), pool_config.as_ref())
                    .unwrap();
            }
            Err(e) => {
                error!("skipping {guid:?}: {e:?}");
            }
        }
    }
}
