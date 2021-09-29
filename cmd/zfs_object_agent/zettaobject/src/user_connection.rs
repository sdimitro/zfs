use crate::object_access::ObjectAccess;
use crate::pool::*;
use crate::server::{HandlerReturn, Server};
use anyhow::Result;
use futures::stream::StreamExt;
use lazy_static::lazy_static;
use log::*;
use nvpair::NvList;
use rusoto_s3::S3;
use std::sync::{Arc, Mutex};
use zettacache::base_types::*;
use zettacache::get_tunable;

lazy_static! {
    pub static ref GET_POOLS_QUEUE_DEPTH: usize = get_tunable("get_pools_queue_depth", 100);
}

pub struct UserServerState {}

struct UserConnectionState {}

impl UserServerState {
    fn connection_handler(&self) -> UserConnectionState {
        UserConnectionState {}
    }

    pub fn start(socket_dir: &str) {
        let socket_path = format!("{}/zfs_user_socket", socket_dir);
        let mut server = Server::new(
            &socket_path,
            UserServerState {},
            Box::new(Self::connection_handler),
        );

        UserConnectionState::register(&mut server);

        server.start();
    }
}

impl UserConnectionState {
    fn register(server: &mut Server<UserServerState, UserConnectionState>) {
        server.register_handler("get pools", Box::new(Self::get_pools));
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
        let mut client = ObjectAccess::get_client(endpoint, region_str, credentials_profile);
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

        let response = Arc::new(Mutex::new(NvList::new_unique_names()));
        for buck in buckets {
            let object_access =
                ObjectAccess::from_client(client, buck.as_str(), readonly, endpoint, region_str);
            let guid_result = nvl.lookup_uint64("guid");
            if let Ok(guid) = guid_result {
                if !Pool::exists(&object_access, PoolGuid(guid)).await {
                    client = object_access.release_client();
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
                        client = object_access.release_client();
                        continue;
                    }
                }
            }

            object_access
                .list_prefixes("zfs/")
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
            client = object_access.release_client();
        }
        let owned_response = Arc::try_unwrap(response).unwrap().into_inner().unwrap();
        info!("sending response: {:?}", owned_response);
        Ok(Some(owned_response))
    }
}
