use std::sync::Arc;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use futures::stream::StreamExt;
use log::*;
use nvpair::NvList;
use semver::Version;
use serde::Serialize;
use util::maybe_die_with;
use util::message::*;
use util::tunable;
use zettacache::base_types::*;
use zettacache::ZettaCache;

use crate::object_access::BucketAccess;
use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessProtocol;
use crate::object_access::S3Credentials;
use crate::pool::*;
use crate::pool_destroy;
use crate::server::return_result;
use crate::server::ConnectionState;
use crate::server::FailureMessage;
use crate::server::HandlerReturn;
use crate::server::Server;

tunable! {
    pub static ref GET_POOLS_QUEUE_DEPTH: usize = 100;
}

pub struct PublicServerState {
    cache: Option<ZettaCache>,
}

struct PublicConnectionState {
    cache: Option<ZettaCache>,
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
            cache: self.cache.as_ref().cloned(),
            version: None,
        }
    }

    pub fn start(socket_dir: &str, cache: Option<ZettaCache>) {
        let socket_path = format!("{}/zfs_public_socket", socket_dir);

        let mut server = Server::new(
            &socket_path,
            0o666, // world writable
            PublicServerState { cache },
            Box::new(Self::connection_handler),
            vec![Version::new(1, 0, 0)],
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
        Ok(Box::pin(async move { Self::get_pools_impl(nvl).await }))
    }

    async fn get_pools_impl(nvl: NvList) -> Result<Option<NvList>> {
        // XXX convert to use serde nvlist request and response
        let region_cstr = nvl.lookup_string("region")?;
        let endpoint_cstr = nvl.lookup_string("endpoint")?;
        let region = region_cstr.to_str()?.to_string();
        let endpoint = endpoint_cstr.to_str()?.to_string();
        let _readonly = nvl.exists("readonly");
        let credentials_profile: Option<String> = nvl
            .lookup_string("credentials_profile")
            .ok()
            .map(|s| s.to_string_lossy().to_string());

        let credentials = match credentials_profile {
            Some(profile) => S3Credentials::Profile(profile),
            None => S3Credentials::Automatic,
        };

        let bucket_access = BucketAccess::new(ObjectAccessProtocol::S3 {
            endpoint: endpoint.clone(),
            region: region.clone(),
            credentials: credentials.clone(),
        })
        .await?;

        let mut buckets = vec![];
        let bucket_result = nvl.lookup_string("bucket");
        if let Ok(bucket) = bucket_result {
            buckets.push(bucket.into_string()?);
        } else {
            buckets.append(&mut bucket_access.list_buckets().await);
        }

        maybe_die_with(|| "in get_pools_impl");
        let response = Arc::new(Mutex::new(NvList::new_unique_names()));
        for buck in buckets {
            let object_access = ObjectAccess::new(
                ObjectAccessProtocol::S3 {
                    endpoint: endpoint.clone(),
                    region: region.clone(),
                    credentials: credentials.clone(),
                },
                buck,
                false,
            )
            .await?;
            let guid_result = nvl.lookup_uint64("guid");
            if let Ok(guid) = guid_result {
                if !Pool::exists(&object_access, PoolGuid(guid)).await {
                    continue;
                }

                match Pool::get_config(&object_access, PoolGuid(guid)).await {
                    Ok(pool_config) => {
                        let mut owned_response =
                            Arc::try_unwrap(response).unwrap().into_inner().unwrap();
                        owned_response
                            .insert(format!("{}", guid), pool_config.as_ref())
                            .unwrap();
                        debug!("sending response: {:?}", owned_response);
                        return Ok(Some(owned_response));
                    }
                    Err(e) => {
                        error!("skipping {:?}: {:?}", guid, e);
                        continue;
                    }
                }
            }

            object_access
                .list_prefixes("zfs/".to_string())
                .for_each_concurrent(*GET_POOLS_QUEUE_DEPTH, |prefix| {
                    let my_object_access = object_access.clone();
                    let my_response = response.clone();
                    async move {
                        debug!("prefix: {}", prefix);
                        let split: Vec<&str> = prefix.rsplitn(3, '/').collect();
                        let guid_str = split[1];
                        if let Ok(guid64) = str::parse::<u64>(guid_str) {
                            let guid = PoolGuid(guid64);
                            match Pool::get_config(&my_object_access, guid).await {
                                Ok(pool_config) => my_response
                                    .lock()
                                    .unwrap()
                                    .insert(guid_str, pool_config.as_ref())
                                    .unwrap(),
                                Err(e) => {
                                    error!("skipping {:?}: {:?}", guid, e);
                                }
                            }
                        }
                    }
                })
                .await;
        }
        let owned_response = Arc::try_unwrap(response).unwrap().into_inner().unwrap();
        info!("sending response: {:?}", owned_response);
        Ok(Some(owned_response))
    }

    fn get_destroying_pools(&mut self, nvl: NvList) -> HandlerReturn {
        Ok(Box::pin(async move {
            // XXX convert to use serde nvlist response
            debug!("got request: {:?}", nvl);
            let pools = pool_destroy::get_destroy_list().await;

            let mut response = NvList::new_unique_names();
            response
                .insert(AGENT_RESPONSE_TYPE, TYPE_GET_DESTROYING_POOLS)
                .unwrap();
            response.insert("pools", pools.as_ref()).unwrap();

            debug!("sending response: {:?}", response);
            Ok(Some(response))
        }))
    }

    fn clear_destroyed_pools(&mut self, nvl: NvList) -> HandlerReturn {
        Ok(Box::pin(async move {
            // XXX convert to use serde nvlist response
            debug!("got request: {:?}", nvl);
            pool_destroy::remove_not_in_progress().await;

            let mut response = NvList::new_unique_names();
            response
                .insert(AGENT_RESPONSE_TYPE, TYPE_CLEAR_DESTROYED_POOLS)
                .unwrap();

            debug!("sending response: {:?}", response);
            Ok(Some(response))
        }))
    }

    fn report_hits(&mut self, _: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        debug!("got ReportHitsRequest");
        Ok(Box::pin(async move {
            #[derive(Debug, Serialize)]
            struct ReportHitsResponse {
                started: u64,
                lookups: u64,
                cache_capacity: u64,
                meta_overhead: u64,
                bucket_size: u64,
                live_histogram: Vec<u64>,
                ghost_histogram: Vec<u64>,
            }

            let response = match cache {
                Some(cache) => {
                    let phys = cache.hits_by_size_data().await;
                    Ok(ReportHitsResponse {
                        started: phys.started().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        lookups: phys.lookups,
                        cache_capacity: phys.cache_capacity,
                        meta_overhead: phys.meta_overhead,
                        bucket_size: phys.bucket_size,
                        live_histogram: phys.live_histogram,
                        ghost_histogram: phys.ghost_histogram,
                    })
                }
                None => Err(FailureMessage::new("no zettacache present")),
            };
            return_result(TYPE_REPORT_HITS, (), response, true)
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

            let response = match cache {
                Some(cache) => Ok(ListDevicesResponse {
                    devices_json: cache.devices_as_json(),
                }),
                None => Err(FailureMessage::new("no zettacache present")),
            };
            return_result(TYPE_LIST_DEVICES, (), response, true)
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

            let response = match cache {
                Some(cache) => Ok(ZcacheIostatResponse {
                    iostats_json: cache.io_stats_as_json(),
                }),
                None => Err(FailureMessage::new("no zettacache present")),
            };
            return_result(TYPE_ZCACHE_IOSTAT, (), response, false)
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

            let response = match cache {
                Some(cache) => Ok(ZcacheStatsResponse {
                    stats_json: cache.stats_as_json().await,
                }),
                None => Err(FailureMessage::new("no zettacache present")),
            };
            return_result(TYPE_ZCACHE_STATS, (), response, false)
        }))
    }
}
