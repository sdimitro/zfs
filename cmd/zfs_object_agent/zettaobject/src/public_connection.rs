use crate::object_access::ObjectAccess;
use crate::pool::*;
use crate::pool_destroy;
use crate::server::handler_return_ok;
use crate::server::ConnectionState;
use crate::server::{HandlerReturn, Server};
use anyhow::Result;
use futures::stream::StreamExt;
use lazy_static::lazy_static;
use log::*;
use nvpair::NvList;
use rusoto_s3::S3;
use semver::Version;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use util::get_tunable;
use util::maybe_die_with;
use zettacache::base_types::*;
use zettacache::ZettaCache;

lazy_static! {
    pub static ref GET_POOLS_QUEUE_DEPTH: usize = get_tunable("get_pools_queue_depth", 100);
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
        server.register_handler("get pools", Box::new(Self::get_pools));
        server.register_handler("get destroying pools", Box::new(Self::get_destroying_pools));
        server.register_handler(
            "clear destroyed pools",
            Box::new(Self::clear_destroyed_pools),
        );
        server.register_handler("report_hits", Box::new(Self::report_hits));
        server.register_handler("list_devices", Box::new(Self::list_devices));
        server.register_handler("zcache_iostat", Box::new(Self::zcache_iostat));
        server.register_handler("zcache_stats", Box::new(Self::zcache_stats));
    }

    fn get_pools(&mut self, nvl: NvList) -> HandlerReturn {
        info!("got request: {:?}", nvl);
        Ok(Box::pin(async move { Self::get_pools_impl(nvl).await }))
    }

    async fn get_pools_impl(nvl: NvList) -> Result<Option<NvList>> {
        let region_cstr = nvl.lookup_string("region")?;
        let endpoint_cstr = nvl.lookup_string("endpoint")?;
        let region_str = region_cstr.to_str()?;
        let endpoint = endpoint_cstr.to_str()?;
        let readonly = nvl.exists("readonly");
        let credentials_profile: Option<String> = nvl
            .lookup_string("credentials_profile")
            .ok()
            .map(|s| s.to_string_lossy().to_string());
        let client = ObjectAccess::get_client(endpoint, region_str, credentials_profile);
        let mut buckets = vec![];
        let bucket_result = nvl.lookup_string("bucket");
        if let Ok(bucket) = bucket_result {
            buckets.push(bucket.into_string()?);
        } else {
            buckets.append(
                &mut client
                    .list_buckets()
                    .await?
                    .buckets
                    .unwrap()
                    .into_iter()
                    .map(|b| b.name.unwrap())
                    .collect(),
            );
        }

        maybe_die_with(|| "in get_pools_impl");
        let response = Arc::new(Mutex::new(NvList::new_unique_names()));
        for buck in buckets {
            let object_access = Arc::new(ObjectAccess::from_client(
                client.clone(),
                buck.as_str(),
                readonly,
                endpoint,
                region_str,
            ));
            let guid_result = nvl.lookup_uint64("GUID");
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
            debug!("got request: {:?}", nvl);
            let pools = pool_destroy::get_destroy_list().await;

            let mut response = NvList::new_unique_names();
            response
                .insert("Type", "get destroying pools done")
                .unwrap();
            response.insert("pools", pools.as_ref()).unwrap();

            debug!("sending response: {:?}", response);
            Ok(Some(response))
        }))
    }

    fn clear_destroyed_pools(&mut self, nvl: NvList) -> HandlerReturn {
        Ok(Box::pin(async move {
            debug!("got request: {:?}", nvl);
            pool_destroy::remove_not_in_progress().await;

            let mut response = NvList::new_unique_names();
            response
                .insert("Type", "clear destroying pools done")
                .unwrap();

            debug!("sending response: {:?}", response);
            Ok(Some(response))
        }))
    }

    fn report_hits(&mut self, nvl: NvList) -> HandlerReturn {
        debug!("got request: {:?}", nvl);
        let mut response = NvList::new_unique_names();
        let cache = self.cache.as_ref().cloned();
        match cache {
            Some(zettacache) => Ok(Box::pin(async move {
                response.insert("Type", "report_hits").unwrap();
                let size_data = zettacache.hits_by_size_data().await;
                response
                    .insert("histogram", &size_data.histogram[..])
                    .unwrap();
                response
                    .insert("cache_capacity", &size_data.cache_capacity)
                    .unwrap();
                response
                    .insert("bucket_size", &size_data.bucket_size)
                    .unwrap();
                response.insert("lookups", &size_data.lookups).unwrap();
                let started = size_data
                    .started()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                response.insert("started", &started).unwrap();
                response.insert("result", "ok").unwrap();
                debug!("sending response: {:?}", response);
                Ok(Some(response))
            })),
            None => {
                response.insert("Type", "report_hits").unwrap();
                response.insert("result", "err").unwrap();
                debug!("sending response: {:?}", response);
                handler_return_ok(Some(response))
            }
        }
    }

    fn list_devices(&mut self, nvl: NvList) -> HandlerReturn {
        debug!("got request: {:?}", nvl);
        let mut response = NvList::new_unique_names();
        let cache = self.cache.as_ref().cloned();

        if let Some(zettacache) = cache {
            Ok(Box::pin(async move {
                response.insert("Type", "list_devices").unwrap();
                response.insert("result", "ok").unwrap();
                response
                    .insert("devices_json", zettacache.devices_as_json().as_str())
                    .unwrap();

                debug!("sending response: {:?}", response);
                Ok(Some(response))
            }))
        } else {
            Ok(Box::pin(async move {
                response.insert("Type", "list_devices").unwrap();
                response.insert("result", "err").unwrap();
                debug!("sending response: {:?}", response);
                Ok(Some(response))
            }))
        }
    }

    fn zcache_iostat(&mut self, nvl: NvList) -> HandlerReturn {
        debug!("got request: {:?}", nvl);
        let mut response = NvList::new_unique_names();
        let cache = self.cache.as_ref().cloned();

        if let Some(zettacache) = cache {
            Ok(Box::pin(async move {
                let json_stats = zettacache.io_stats_as_json();

                response
                    .insert("iostats_json", &json_stats.as_str())
                    .unwrap();
                response.insert("Type", "zcache_iostat").unwrap();
                response.insert("result", "ok").unwrap();

                debug!("sending response: {:?}", response);
                Ok(Some(response))
            }))
        } else {
            Ok(Box::pin(async move {
                response.insert("Type", "zcache_iostat").unwrap();
                response.insert("result", "err").unwrap();
                debug!("sending response: {:?}", response);
                Ok(Some(response))
            }))
        }
    }

    fn zcache_stats(&mut self, nvl: NvList) -> HandlerReturn {
        debug!("got request: {:?}", nvl);
        let mut response = NvList::new_unique_names();
        let cache = self.cache.as_ref().cloned();

        if let Some(zettacache) = cache {
            Ok(Box::pin(async move {
                let json_stats = zettacache.stats_as_json().await;

                response.insert("stats_json", &json_stats[..]).unwrap();
                response.insert("Type", "zcache_stats").unwrap();
                response.insert("result", "ok").unwrap();

                debug!("sending response: {:?}", response);
                Ok(Some(response))
            }))
        } else {
            Ok(Box::pin(async move {
                response.insert("Type", "zcache_stats").unwrap();
                response.insert("result", "err").unwrap();
                debug!("sending response: {:?}", response);
                Ok(Some(response))
            }))
        }
    }
}
