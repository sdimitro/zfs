use std::collections::HashMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use bytes::Bytes;
use derivative::Derivative;
use futures::future;
use log::*;
use nvpair::NvList;
use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use util::maybe_die_with;
use util::measure;
use util::message::*;
use util::super_trace;
use util::tunable;
use util::AlignedVec;
use uuid::Uuid;
use zettacache::base_types::*;
use zettacache::ZettaCache;

use crate::access_stats::StatMapValue;
use crate::base_types::*;
use crate::features::FeatureError;
use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessProtocol;
use crate::pool::*;
use crate::pool_destroy;
use crate::server::handler_return_ok;
use crate::server::return_result;
use crate::server::ConnectionState;
use crate::server::FailureMessage;
use crate::server::HandlerReturn;
use crate::server::Responder;
use crate::server::SerialHandlerReturn;
use crate::server::Server;

tunable! {
    pub static ref DIE_BEFORE_END_TXG_RESPONSE_PCT: f64 = 0.0;
}

pub struct RootServerState {
    cache: Option<ZettaCache>,
    id: Uuid,
}

struct RootConnectionState {
    pool: Option<Arc<Pool>>,
    cache: Option<ZettaCache>,
    id: Uuid,
    version: Option<Version>,
}

impl ConnectionState for RootConnectionState {
    fn set_version(&mut self, version: Version) {
        assert!(self.version.is_none());
        self.version = Some(version);
    }
}

impl RootServerState {
    fn connection_handler(&self) -> RootConnectionState {
        RootConnectionState {
            pool: None,
            cache: self.cache.as_ref().cloned(),
            id: self.id,
            version: None,
        }
    }

    pub fn start(socket_dir: &Path, cache: Option<ZettaCache>, id: Uuid) {
        let socket_path = socket_dir.join("zfs_root_socket");
        let mut server = Server::new(
            &socket_path,
            0o600,
            RootServerState { cache, id },
            Box::new(Self::connection_handler),
            vec![Version::new(1, 0, 0), Version::new(1, 1, 0)],
        );

        RootConnectionState::register(&mut server);
        server.start();
    }
}
#[derive(Deserialize, Debug)]
struct ObjectAccessRequest {
    bucket: String,
    #[serde(default)]
    readonly: bool,
    #[serde(flatten)]
    protocol: ObjectAccessProtocol,
}

impl ObjectAccessRequest {
    async fn object_access(self) -> Result<Arc<ObjectAccess>> {
        ObjectAccess::new(self.protocol, self.bucket, self.readonly).await
    }
}

impl RootConnectionState {
    fn register(server: &mut Server<RootServerState, RootConnectionState>) {
        server.register_serial_handler(TYPE_CREATE_POOL, Box::new(Self::create_pool));
        server.register_serial_handler(TYPE_OPEN_POOL, Box::new(Self::open_pool));
        server.register_serial_handler(TYPE_RESUME_COMPLETE, Box::new(Self::resume_complete));
        server.register_handler(TYPE_BEGIN_TXG, Box::new(Self::begin_txg));
        server.register_handler(TYPE_FLUSH_WRITES, Box::new(Self::flush_writes));
        server.register_handler(TYPE_END_TXG, Box::new(Self::end_txg));
        server.register_handler(TYPE_FREE_BLOCKS, Box::new(Self::free_blocks));
        server.register_handler(TYPE_GET_STATS, Box::new(Self::get_stats));
        server.register_handler(TYPE_CLOSE_POOL, Box::new(Self::close_pool));
        server.register_handler(TYPE_EXIT_AGENT, Box::new(Self::exit_agent));
        server.register_handler(TYPE_ENABLE_FEATURE, Box::new(Self::enable_feature));
        server.register_handler(
            TYPE_RESUME_DESTROY_POOL,
            Box::new(Self::resume_destroy_pool),
        );
        server.register_handler(TYPE_CLEAR_HIT_DATA, Box::new(Self::clear_hit_data));
        server.register_handler(TYPE_ADD_DISK, Box::new(Self::add_disk));
        server.register_handler(TYPE_SYNC_CHECKPOINT, Box::new(Self::sync_checkpoint));
        server.register_handler(TYPE_INITIATE_MERGE, Box::new(Self::initiate_merge));
        server.register_struct_handler(MessageType::ReadBlock, Box::new(Self::read_block));
        server.register_struct_handler(MessageType::WriteBlock, Box::new(Self::write_block));
    }

    fn create_pool(&mut self, nvl: NvList) -> SerialHandlerReturn {
        Box::pin(async move {
            #[derive(Debug, Deserialize)]
            struct CreatePoolRequest {
                #[serde(flatten)]
                id: RequestId,
                #[serde(flatten)]
                object_access: ObjectAccessRequest,
                name: String,
            }
            #[derive(Debug, Serialize, Deserialize)]
            struct RequestId {
                guid: PoolGuid,
            }

            let request: CreatePoolRequest = nvpair::from_nvlist(&nvl)?;
            info!("got {:?}", request);
            let object_access = request.object_access.object_access().await?;
            let result = Pool::create(&object_access, &request.name, request.id.guid)
                .await
                .map_err(FailureMessage::new);

            return_result(TYPE_CREATE_POOL, request.id, result, true)
        })
    }

    fn open_pool(&mut self, nvl: NvList) -> SerialHandlerReturn {
        Box::pin(async move {
            #[derive(Debug, Serialize, Deserialize)]
            struct RequestId {
                guid: PoolGuid,
            }
            #[derive(Debug, Deserialize)]
            struct OpenPoolRequest {
                #[serde(flatten)]
                object_access: ObjectAccessRequest,
                #[serde(flatten)]
                id: RequestId,
                #[serde(default)]
                rollback: bool,
                txg: Option<Txg>,
                syncing_txg: Option<Txg>,
            }

            let request: OpenPoolRequest = nvpair::from_nvlist(&nvl)?;
            info!("got {:?}", request);

            #[derive(Debug, Serialize)]
            #[serde(tag = "err")]
            enum Failure {
                Mmp {
                    hostname: String,
                },
                Feature {
                    invalid_features: HashMap<String, String>,
                    can_readonly: bool,
                },
                Io {
                    message: String,
                },
                Checkpoint,
            }

            let object_access = request.object_access.object_access().await?;
            let result = match Pool::open(
                object_access,
                request.id.guid,
                request.txg,
                self.cache.as_ref().cloned(),
                self.id,
                request.syncing_txg,
                request.rollback,
            )
            .await
            {
                Err(PoolOpenError::Mmp(hostname)) => Err(Failure::Mmp { hostname }),
                Err(PoolOpenError::Feature(FeatureError {
                    features,
                    can_readonly,
                })) => Err(Failure::Feature {
                    invalid_features: features
                        .into_iter()
                        .map(|feature| (feature.name, "".to_string()))
                        .collect(),
                    can_readonly,
                }),
                Err(PoolOpenError::Get(e)) => {
                    /*
                     * It would be really nice to bring up the exact error type from the
                     * object_access layer here and case on it properly. Unfortunately,
                     * attempting to do so is best described as... fraught. Errors come from a
                     * number of sources, and are implicitly converted frequently. Ultimately,
                     * the blocker is that most of the error types produced by our dependencies
                     * do not implement Clone, and so cannot be easily propogated up the chain
                     * when multiple people may be fetching the same object.
                     *
                     * If we ever decide to implement our own error types instead of using the
                     * underlying ones, we could handle this situation more cleanly. Until
                     * then, we just pass the root cause error message back to the kernel, and
                     * hope that it can present a usable error to the user.
                     */
                    Err(Failure::Io {
                        message: e.root_cause().to_string(),
                    })
                }
                Err(PoolOpenError::NoCheckpoint) => Err(Failure::Checkpoint),
                Ok((pool, uber, next_block)) => {
                    self.pool = Some(Arc::new(pool));

                    #[derive(Debug, Serialize)]
                    struct Success {
                        #[serde(flatten)]
                        existing: Option<Existing>,
                        next_block: BlockId,
                    }
                    #[derive(Serialize, Derivative)]
                    #[derivative(Debug)]
                    struct Existing {
                        #[serde(with = "serde_bytes")]
                        #[derivative(Debug = "ignore")]
                        uberblock: Vec<u8>,
                        #[serde(with = "serde_bytes")]
                        #[derivative(Debug = "ignore")]
                        config: Vec<u8>,
                        features: HashMap<String, u64>,
                    }

                    Ok(Success {
                        next_block,
                        existing: uber.map(|uber| Existing {
                            uberblock: uber.zfs_uberblock().to_owned(),
                            config: uber.zfs_config().to_owned(),
                            features: uber
                                .features()
                                .iter()
                                .map(|(feature, refcount)| (feature.name.clone(), *refcount))
                                .collect(),
                        }),
                    })
                }
            };
            return_result(TYPE_OPEN_POOL, request.id, result, true)
        })
    }

    fn begin_txg(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct BeginTxgRequest {
            txg: Txg,
        }
        let request: BeginTxgRequest = nvpair::from_nvlist(&nvl)?;
        debug!("got {:?}", request);
        let pool = self.pool.as_ref().ok_or_else(|| anyhow!("no pool open"))?;
        pool.begin_txg(request.txg);

        handler_return_ok(None)
    }

    fn resume_complete(&mut self, _nvl: NvList) -> SerialHandlerReturn {
        info!("got ResumeComplete");

        // This is .await'ed by the server's thread, so we can't see any new writes
        // come in while it's in progress.
        Box::pin(async move {
            let pool = self.pool.as_ref().ok_or_else(|| anyhow!("no pool open"))?;
            pool.resume_complete().await;
            Ok(None)
        })
    }

    fn flush_writes(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct FlushWritesRequest {
            block: BlockId,
        }
        let request: FlushWritesRequest = nvpair::from_nvlist(&nvl)?;
        debug!("got {:?}", request);
        let pool = self.pool.as_ref().ok_or_else(|| anyhow!("no pool open"))?;
        pool.initiate_flush(request.block);
        handler_return_ok(None)
    }

    fn end_txg(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Derivative)]
        #[derivative(Debug)]
        struct EndTxgRequest<'a> {
            #[serde(with = "serde_bytes")]
            // We're careful here to avoid dumping the "uberblock" and "config" fields to avoid
            // filling the log unnecessarily.
            #[derivative(Debug = "ignore")]
            uberblock: &'a [u8],
            #[serde(with = "serde_bytes")]
            #[derivative(Debug = "ignore")]
            config: &'a [u8],
            checkpoint: Option<Txg>,
            // XXX kernel also sends TXG, which we ignore; we should remove it from the API.
        }

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("no pool open"))?
            .clone();
        Ok(Box::pin(async move {
            let request: EndTxgRequest = nvpair::from_nvlist(&nvl)?;
            debug!("got {:?}", request);
            // XXX change kernel to not send this field if it doesn't want to take a checkpoint
            let checkpoint_txg = match &request.checkpoint {
                Some(Txg(0)) | None => None,
                Some(txg) => Some(*txg),
            };

            let (stats, features) = pool
                .end_txg(
                    request.uberblock.to_owned(),
                    request.config.to_owned(),
                    checkpoint_txg,
                )
                .await;
            #[derive(Debug, Serialize)]
            struct EndTxgResponse {
                response_type: &'static str,
                #[serde(flatten)]
                stats: PoolStatsPhys,
                features: HashMap<String, u64>,
            }
            let response = EndTxgResponse {
                response_type: TYPE_END_TXG,
                stats,
                features: features
                    .into_iter()
                    .map(|(flag, refcount)| (flag.name, refcount))
                    .collect(),
            };
            return_struct(response, true)
        }))
    }

    /// queue write, sends response when completed (persistent).
    /// completion may not happen until flush_pool() is called
    fn write_block(
        &mut self,
        responder: Responder,
        write_block_request_slice: &[u8],
        data: AlignedVec,
    ) -> Result<()> {
        let request: WriteBlockRequest = slice_to_struct(write_block_request_slice);

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("no pool open"))?
            .clone();

        pool.write_block(
            BlockId(request.block),
            data.into(),
            Box::new(move || {
                let response = WriteBlockResponse {
                    block: request.block,
                    token: request.token,
                };
                responder.respond_with_struct(MessageType::WriteBlock, &response, Bytes::new());
            }),
        );
        Ok(())
    }

    fn free_blocks(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct FreeBlocksRequest {
            block: Vec<u64>,
            size: Vec<u32>,
        }
        let request: FreeBlocksRequest = nvpair::from_nvlist(&nvl)?;
        debug!("got FreeBlocksRequest({} entries)", request.block.len());

        let pool = self.pool.as_ref().ok_or_else(|| anyhow!("no pool open"))?;
        pool.free_blocks(&request.block, &request.size);
        maybe_die_with(|| "after free block request".to_string());
        handler_return_ok(None)
    }

    fn read_block(
        &mut self,
        responder: Responder,
        read_block_request_slice: &[u8],
        request_payload: AlignedVec,
    ) -> Result<()> {
        assert_eq!(request_payload.len(), 0);
        let request: ReadBlockRequest = slice_to_struct(read_block_request_slice);
        let heal = request.heal();

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("no pool open"))?
            .clone();
        measure!("RootConnectionState::read_block").spawn(async move {
            let mut data = pool.read_block(BlockId(request.block), heal).await;

            //
            // If the cache has the wrong content/size for this BlockId, then proactively do a
            // healing read from the object store.
            //
            if !heal && data.len() != request.size as usize {
                debug!(
                    "read size mismatch: expected={} actual={}",
                    request.size,
                    data.len()
                );
                data = pool.read_block(BlockId(request.block), true).await;
            }
            let response = ReadBlockResponse {
                block: request.block,
                token: request.token,
            };
            responder.respond_with_struct(MessageType::ReadBlock, &response, data);
        });
        Ok(())
    }

    fn get_stats(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct GetStatsRequest {
            token: u64,
        }
        let request: GetStatsRequest = nvpair::from_nvlist(&nvl)?;
        trace!("got {:?}", request);

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("no pool open"))?
            .clone();

        //
        // Build an nvlist from the stats hash map
        // Each map entry can be a Counter, a CounterMap, or a Histogram
        //
        let stats = pool.state.shared_state.object_access.collect_stats();
        // XXX convert response to use serde nvlist
        let mut nvl = NvList::new_unique_names();
        for (name, stat_value) in stats.iter() {
            match stat_value {
                StatMapValue::Counter(timestamp) => nvl.insert(name, timestamp).unwrap(),
                StatMapValue::CounterMap(cm) => {
                    let mut contents = NvList::new_unique_names();
                    for (key, value) in cm.iter() {
                        // e.g. "operations": 459
                        contents.insert(key, value).unwrap();
                    }
                    // e.g. "MetadataPut": {"operations": 459, "total_bytes": 350280, "active": 2}
                    nvl.insert(name, contents.as_ref()).unwrap();
                }
                StatMapValue::Histogram(histogram) => nvl.insert(name, &histogram[..]).unwrap(),
            }
        }

        let mut response = NvList::new_unique_names();
        response.insert("response_type", TYPE_GET_STATS).unwrap();
        response.insert("token", &request.token).unwrap();
        response.insert("stats", nvl.as_ref()).unwrap();

        trace!("sending stats done response: {:?}", response);
        handler_return_ok(Some(response))
    }

    fn close_pool(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct ClosePoolRequest {
            #[serde(default)]
            destroy: bool,
        }
        let request: ClosePoolRequest = nvpair::from_nvlist(&nvl)?;
        info!("got {:?}", request);

        let pool_opt = self.pool.take();
        Ok(Box::pin(async move {
            if let Some(pool) = pool_opt {
                Arc::try_unwrap(pool)
                    .map_err(|_| {
                        anyhow!("pool close request while there are other operations in progress")
                    })?
                    .close(request.destroy)
                    .await;
            }
            #[derive(Debug, Serialize)]
            struct ClosePoolResponse {
                response_type: &'static str,
            }
            let response = ClosePoolResponse {
                response_type: TYPE_CLOSE_POOL,
            };
            return_struct(response, true)
        }))
    }

    // XXX This doesn't actually exit the agent, it just closes the connection,
    // which the kernel could do from its end.  It's unclear what the kernel
    // really wants.
    fn exit_agent(&mut self, nvl: NvList) -> HandlerReturn {
        info!("got request: {:?}", nvl);
        Err(anyhow!("exit requested"))
    }

    fn enable_feature(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct EnableFeatureRequest {
            feature: String,
        }
        let request: EnableFeatureRequest = nvpair::from_nvlist(&nvl)?;
        info!("got {:?}", request);
        let pool = self
            .pool
            .as_ref()
            .expect("Attempted to set feature with no pool")
            .clone();
        pool.enable_feature(&request.feature);

        #[derive(Debug, Serialize)]
        struct EnableFeatureResponse {
            response_type: &'static str,
            feature: String,
        }
        let response = EnableFeatureResponse {
            response_type: TYPE_ENABLE_FEATURE,
            feature: request.feature,
        };
        handler_return_struct(response, true)
    }

    fn resume_destroy_pool(&mut self, nvl: NvList) -> HandlerReturn {
        Ok(Box::pin(async move {
            #[derive(Deserialize, Debug)]
            struct ResumeDestroyPoolRequest {
                #[serde(flatten)]
                object_access: ObjectAccessRequest,
                guid: PoolGuid,
            }

            let request: ResumeDestroyPoolRequest = nvpair::from_nvlist(&nvl)?;
            debug!("got {:?}", request);
            let object_access = request.object_access.object_access().await?;
            let result = pool_destroy::resume_destroy(object_access, request.guid)
                .await
                .map_err(FailureMessage::new);
            return_result(TYPE_RESUME_DESTROY_POOL, (), result, true)
        }))
    }

    fn clear_hit_data(&mut self, _nvl: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        Ok(Box::pin(async move {
            #[derive(Debug, Serialize)]
            struct ClearHitDataResponse {
                response_type: &'static str,
                result: &'static str,
            }

            let result = match cache {
                Some(cache) => {
                    debug!("got ClearHitDataRequest");
                    cache.clear_hit_data().await;
                    Ok(())
                }
                None => {
                    debug!("got ClearHitDataRequest, no zettacache present");
                    Err(FailureMessage::new("zettacache not present"))
                }
            };
            // XXX standardize on if response has the same type as request, or with "done" appended
            return_result(TYPE_CLEAR_HIT_DATA, (), result, true)
        }))
    }

    fn add_disk(&mut self, nvl: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        Ok(Box::pin(async move {
            let request: AddDiskRequest = nvpair::from_nvlist(&nvl)?;
            debug!("got {:?}", request);

            let result = match cache {
                Some(cache) => Ok(cache.add_disk(&request.path).await?),
                None => Err(FailureMessage::new("zettacache not present")),
            };
            return_result(TYPE_ADD_DISK, (), result, true)
        }))
    }

    fn sync_checkpoint(&mut self, nvl: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        Ok(Box::pin(async move {
            debug!("got {:?}", nvl);

            let result = match cache {
                Some(cache) => {
                    cache.sync_checkpoint().await;
                    Ok(())
                }
                None => Err(FailureMessage::new("zettacache not present")),
            };
            return_result(TYPE_SYNC_CHECKPOINT, (), result, true)
        }))
    }

    fn initiate_merge(&mut self, nvl: NvList) -> HandlerReturn {
        let cache = self.cache.clone();
        Ok(Box::pin(async move {
            debug!("got {:?}", nvl);

            let result = match cache {
                Some(cache) => {
                    cache.initiate_merge().await;
                    Ok(())
                }
                None => Err(FailureMessage::new("zettacache not present")),
            };
            return_result(TYPE_INITIATE_MERGE, (), result, true)
        }))
    }
}

fn return_struct<T>(response: T, debug: bool) -> Result<Option<NvList>>
where
    T: Debug + Serialize,
{
    if debug {
        trace!("sending response: {:?}", response);
    } else {
        super_trace!("sending response: {:?}", response);
    }
    let nvl = nvpair::to_nvlist(&response)?;
    if debug {
        maybe_die_with(|| format!("before sending response: {:?}", response));
        debug!("sending response nvl: {:?}", nvl);
    } else {
        super_trace!("sending response nvl: {:?}", nvl);
    }
    Ok(Some(nvl))
}

fn handler_return_struct<T>(response: T, debug: bool) -> HandlerReturn
where
    T: Debug + Serialize,
{
    Ok(Box::pin(future::ready(return_struct(response, debug))))
}
