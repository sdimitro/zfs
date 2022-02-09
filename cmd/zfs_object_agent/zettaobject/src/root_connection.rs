use crate::base_types::*;
use crate::features::FeatureError;
use crate::object_access::{ObjectAccess, StatMapValue};
use crate::pool::*;
use crate::pool_destroy;
use crate::server::HandlerReturn;
use crate::server::SerialHandlerReturn;
use crate::server::Server;
use crate::server::{handler_return_ok, ConnectionState};
use anyhow::anyhow;
use anyhow::Result;
use derivative::Derivative;
use futures::future;
use lazy_static::lazy_static;
use log::*;
use nvpair::NvList;
use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use util::AlignedBytes;
use util::From64;
use util::{get_tunable, super_trace, with_alloctag};
use util::{maybe_die_with, with_alloctag_hf};
use uuid::Uuid;
use zettacache::base_types::*;
use zettacache::ZettaCache;

lazy_static! {
    pub static ref DIE_BEFORE_END_TXG_RESPONSE_PCT: f64 =
        get_tunable("die_before_end_txg_response_pct", 0.0);
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

    pub fn start(socket_dir: &str, cache: Option<ZettaCache>, id: Uuid) {
        let socket_path = format!("{}/zfs_root_socket", socket_dir);
        let mut server = Server::new(
            &socket_path,
            0o600,
            RootServerState { cache, id },
            Box::new(Self::connection_handler),
            vec![Version::new(1, 0, 0)],
        );

        RootConnectionState::register(&mut server);
        server.start();
    }
}
#[derive(Deserialize, Debug)]
struct ObjectAccessRequest {
    bucket: String,
    region: String,
    endpoint: String,
    #[serde(default)]
    readonly: bool,
    credentials_profile: Option<String>,
}
impl ObjectAccessRequest {
    fn object_access(&self) -> Arc<ObjectAccess> {
        ObjectAccess::new(
            &self.endpoint,
            &self.region,
            &self.bucket,
            self.credentials_profile.clone(),
            self.readonly,
        )
    }
}

impl RootConnectionState {
    fn register(server: &mut Server<RootServerState, RootConnectionState>) {
        server.register_serial_handler("create pool", Box::new(Self::create_pool));
        server.register_serial_handler("open pool", Box::new(Self::open_pool));
        server.register_serial_handler("resume complete", Box::new(Self::resume_complete));
        server.register_handler("begin txg", Box::new(Self::begin_txg));
        server.register_handler("flush writes", Box::new(Self::flush_writes));
        server.register_handler("end txg", Box::new(Self::end_txg));
        server.register_handler("write block", Box::new(Self::write_block));
        server.register_handler("free blocks", Box::new(Self::free_blocks));
        server.register_handler("read block", Box::new(Self::read_block));
        server.register_handler("get stats", Box::new(Self::get_stats));
        server.register_handler("close pool", Box::new(Self::close_pool));
        server.register_handler("exit agent", Box::new(Self::exit_agent));
        server.register_handler("enable feature", Box::new(Self::enable_feature));
        server.register_handler("resume destroy pool", Box::new(Self::resume_destroy_pool));
        server.register_handler("clear_hit_data", Box::new(Self::clear_hit_data));
    }

    fn create_pool(&mut self, nvl: NvList) -> SerialHandlerReturn {
        Box::pin(async move {
            #[derive(Deserialize, Debug)]
            struct CreatePoolRequest {
                #[serde(flatten)]
                object_access: ObjectAccessRequest,
                #[serde(rename = "GUID")]
                guid: PoolGuid,
                name: String,
            }
            let request: CreatePoolRequest = nvpair::from_nvlist(&nvl)?;
            info!("got {:?}", request);

            let mut error = None;
            if let Err(err) = Pool::create(
                &request.object_access.object_access(),
                &request.name,
                request.guid,
            )
            .await
            {
                error!("pool create failed: {:?}", &err);
                error = Some(err.to_string().replace('\n', ""));
            }
            #[derive(Debug, Serialize)]
            struct CreatePoolResponse {
                #[serde(rename = "Type")]
                response_type: &'static str,
                #[serde(rename = "GUID")]
                guid: PoolGuid,
                cause: Option<String>,
            }
            let response = CreatePoolResponse {
                response_type: "pool create done",
                guid: request.guid,
                cause: error,
            };
            return_struct(response, true)
        })
    }

    fn open_pool(&mut self, nvl: NvList) -> SerialHandlerReturn {
        Box::pin(async move {
            #[derive(Deserialize, Debug)]
            struct OpenPoolRequest {
                #[serde(flatten)]
                object_access: ObjectAccessRequest,
                #[serde(rename = "GUID")]
                guid: PoolGuid,
                #[serde(default)]
                rollback: bool,
                #[serde(rename = "TXG")]
                txg: Option<Txg>,
                syncing_txg: Option<Txg>,
            }
            let request: OpenPoolRequest = nvpair::from_nvlist(&nvl)?;
            info!("got {:?}", request);

            // XXX convert response to use serde nvlist
            let mut response = NvList::new_unique_names();
            response.insert("Type", "pool open done").unwrap();
            response.insert("GUID", &request.guid.0).unwrap();

            let (pool, phys_opt, next_block) = match Pool::open(
                request.object_access.object_access(),
                request.guid,
                request.txg,
                self.cache.as_ref().cloned(),
                self.id,
                request.syncing_txg,
                request.rollback,
            )
            .await
            {
                Err(PoolOpenError::Mmp(hostname)) => {
                    response.insert("cause", "MMP").unwrap();
                    response.insert("hostname", hostname.as_str()).unwrap();
                    debug!("sending response: {:?}", response);
                    return Ok(Some(response));
                }
                Err(PoolOpenError::Feature(FeatureError { features, readonly })) => {
                    response.insert("cause", "feature").unwrap();
                    let mut feature_nvl = NvList::new_unique_names();
                    for feature in features {
                        feature_nvl.insert(feature.name, "").unwrap();
                    }
                    response.insert("features", feature_nvl.as_ref()).unwrap();
                    response.insert("can_readonly", &readonly).unwrap();
                    debug!("sending response: {:?}", response);
                    return Ok(Some(response));
                }
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
                    response.insert("cause", "IO").unwrap();
                    response
                        .insert("message", e.root_cause().to_string().as_str())
                        .unwrap();
                    debug!("sending response: {:?}", response);
                    return Ok(Some(response));
                }
                Err(PoolOpenError::NoCheckpoint) => {
                    response.insert("cause", "checkpoint").unwrap();
                    debug!("sending response: {:?}", response);
                    return Ok(Some(response));
                }
                Ok(x) => x,
            };

            if let Some(phys) = phys_opt {
                response
                    .insert("uberblock", &phys.get_zfs_uberblock()[..])
                    .unwrap();
                response
                    .insert("config", &phys.get_zfs_config()[..])
                    .unwrap();
                let mut feature_nvl = NvList::new_unique_names();
                for (feature, refcount) in phys.features() {
                    feature_nvl.insert(&feature.name, refcount).unwrap();
                }
                response.insert("features", feature_nvl.as_ref()).unwrap();
            }

            response.insert("next_block", &next_block.0).unwrap();

            self.pool = Some(Arc::new(pool));
            maybe_die_with(|| format!("before sending response: {:?}", response));
            debug!("sending response: {:?}", response);
            Ok(Some(response))
        })
    }

    fn begin_txg(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct BeginTxgRequest {
            #[serde(rename = "TXG")]
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
            // We're careful here to avoid dumping the "uberblock" and "config" fields to avoid filling the log unnecessarily.
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
                #[serde(rename = "Type")]
                response_type: &'static str,
                #[serde(flatten)]
                stats: PoolStatsPhys,
                features: HashMap<String, u64>,
            }
            let response = EndTxgResponse {
                response_type: "end txg done",
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
    fn write_block(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Derivative)]
        #[derivative(Debug)]
        struct WriteBlockRequest<'a> {
            block: BlockId,
            #[serde(with = "serde_bytes")]
            #[derivative(Debug = "ignore")]
            data: &'a [u8],
            request_id: u64,
            token: u64,
            #[serde(default)]
            reissue: bool,
            // XXX kernel also includes the write size, which is not needed.
        }
        let request: WriteBlockRequest = nvpair::from_nvlist(&nvl)?;
        super_trace!("got request struct: {:?}", request);

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("no pool open"))?
            .clone();
        let alignment = match self.cache.as_ref() {
            Some(cache) => cache.sector_size(),
            None => 1,
        };
        // XXX copying data
        let bytes = with_alloctag("write_block()", || {
            AlignedBytes::copy_from_slice(request.data, alignment)
        });
        Ok(with_alloctag_hf(
            "write_block() Box::pin({closure})",
            || {
                Box::pin(async move {
                    pool.write_block(request.block, bytes).await;
                    #[derive(Debug, Serialize)]
                    struct WriteBlockResponse {
                        #[serde(rename = "Type")]
                        response_type: &'static str,
                        block: BlockId,
                        request_id: u64,
                        token: u64,
                    }
                    let response = WriteBlockResponse {
                        response_type: "write done",
                        block: request.block,
                        request_id: request.request_id,
                        token: request.token,
                    };
                    if request.reissue {
                        maybe_die_with(|| "after reissued write block request".to_string());
                    }
                    return_struct(response, false)
                })
            },
        ))
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

    fn read_block(&mut self, nvl: NvList) -> HandlerReturn {
        #[derive(Deserialize, Debug)]
        struct ReadBlockRequest {
            size: u64,
            block: BlockId,
            request_id: u64,
            token: u64,
            #[serde(default)]
            heal: bool,
        }
        let request: ReadBlockRequest = nvpair::from_nvlist(&nvl)?;
        super_trace!("got {:?}", request);

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| anyhow!("no pool open"))?
            .clone();
        Ok(Box::pin(async move {
            let mut data = pool.read_block(request.block, request.heal).await;

            //
            // If the cache has the wrong content/size for this BlockId, then proactively do a healing read
            // from the object store.
            //
            if !request.heal && data.len() != usize::from64(request.size) {
                debug!(
                    "read size mismatch: expected={} actual={}",
                    request.size,
                    data.len()
                );
                data = pool.read_block(request.block, true).await;
            }
            #[derive(Serialize, Derivative)]
            #[derivative(Debug)]
            struct ReadBlockResponse<'a> {
                #[serde(rename = "Type")]
                response_type: &'static str,
                block: BlockId,
                request_id: u64,
                token: u64,
                #[serde(with = "serde_bytes")]
                #[derivative(Debug = "ignore")]
                data: &'a [u8],
            }
            let response = ReadBlockResponse {
                response_type: "read done",
                block: request.block,
                request_id: request.request_id,
                token: request.token,
                data: &data,
            };

            return_struct(response, false)
        }))
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
        response.insert("Type", "get stats done").unwrap();
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
                #[serde(rename = "Type")]
                response_type: &'static str,
            }
            let response = ClosePoolResponse {
                response_type: "pool close done",
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
            #[serde(rename = "Type")]
            response_type: &'static str,
            feature: String,
        }
        let response = EnableFeatureResponse {
            response_type: "enable feature done",
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
                #[serde(rename = "GUID")]
                guid: PoolGuid,
            }
            let request: ResumeDestroyPoolRequest = nvpair::from_nvlist(&nvl)?;
            debug!("got {:?}", request);

            #[derive(Debug, Serialize)]
            struct ResumeDestroyPoolResponse {
                #[serde(rename = "Type")]
                response_type: &'static str,
            }
            let response = match pool_destroy::resume_destroy(
                request.object_access.object_access(),
                request.guid,
            )
            .await
            {
                Ok(_) => ResumeDestroyPoolResponse {
                    response_type: "resume destroy pool done",
                },
                Err(error) => {
                    error!("resume destroy pool failed, {:?}", error);
                    ResumeDestroyPoolResponse {
                        response_type: "resume destroy pool failed",
                    }
                }
            };

            return_struct(response, true)
        }))
    }

    fn clear_hit_data(&mut self, _nvl: NvList) -> HandlerReturn {
        #[derive(Debug, Serialize)]
        struct ClearHitDataResponse {
            #[serde(rename = "Type")]
            response_type: &'static str,
            result: &'static str,
        }
        if let Some(cache) = self.cache.as_ref() {
            let cache = cache.clone();
            Ok(Box::pin(async move {
                debug!("got ClearHitDataRequest");

                cache.clear_hit_data().await;
                let response = ClearHitDataResponse {
                    response_type: "clear_hit_data",
                    result: "ok",
                };
                return_struct(response, true)
            }))
        } else {
            debug!("got ClearHitDataRequest, no zettacache present");
            let response = ClearHitDataResponse {
                response_type: "clear_hit_data",
                result: "err",
            };
            handler_return_struct(response, true)
        }
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
