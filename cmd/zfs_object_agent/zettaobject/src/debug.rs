use std::sync::Arc;

use log::debug;
use log::error;
use nix::errno::Errno;
use nvpair::NvList;
use nvpair::NvListRef;
use serde::Serialize;
use tokio::runtime::Handle;
use uuid::Uuid;
use zettacache::base_types::PoolGuid;

use crate::base_types::Txg;
use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessProtocol;
use crate::object_access::S3Credentials;
use crate::pool;
use crate::pool::Pool;
use crate::pool::PoolPhys;
use crate::pool::UberblockPhys;

pub struct DebugHandle {
    runtime: Handle,
    pool: Option<Arc<Pool>>,
    object_access: Option<Arc<ObjectAccess>>,
}

async fn get_object_access(nvl: &NvListRef) -> Arc<ObjectAccess> {
    let bucket_name = nvl.lookup_string("bucket").unwrap();
    let region_str = nvl.lookup_string("region").unwrap();
    let endpoint = nvl.lookup_string("endpoint").unwrap();
    let credentials_profile: Option<String> = nvl
        .lookup_string("credentials_profile")
        .ok()
        .map(|s| s.to_string_lossy().to_string());

    let credentials = match credentials_profile {
        Some(profile) => S3Credentials::Profile(profile),
        None => S3Credentials::Automatic,
    };

    ObjectAccess::new(
        ObjectAccessProtocol::S3 {
            endpoint: endpoint.to_str().unwrap().to_string(),
            region: region_str.to_str().unwrap().to_string(),
            credentials,
        },
        bucket_name.to_str().unwrap().to_string(),
        true,
    )
    .await
    .unwrap()
}

impl DebugHandle {
    pub fn new(runtime: Handle) -> Self {
        DebugHandle {
            runtime,
            pool: None,
            object_access: None,
        }
    }
    pub fn open_pool(&mut self, guid: PoolGuid, nvl: &NvListRef) -> Result<(), Errno> {
        let future = async move {
            let object_access = get_object_access(nvl).await;
            let (pool, _, _) = Pool::open(
                object_access.clone(),
                guid,
                None,
                None,
                Uuid::new_v4(),
                None,
                false,
            )
            .await?;
            Ok((pool, object_access))
        };

        match self.runtime.block_on(future) {
            Ok((pool, object_access)) => {
                self.object_access = Some(object_access);
                self.pool = Some(Arc::new(pool));
                Ok(())
            }
            Err(e) => {
                debug!("failed to open pool in libzoa: {:?}", e);
                match e {
                    pool::PoolOpenError::Feature(_) => Err(Errno::ENOTSUP),
                    pool::PoolOpenError::Get(_) => Err(Errno::EIO),
                    pool::PoolOpenError::Mmp(hostname) => {
                        panic!(
                            "MMP error when readonly open requested; hostname \"{}\"",
                            hostname
                        )
                    }
                    pool::PoolOpenError::NoCheckpoint => {
                        panic!("Checkpoint error when no rollback requested")
                    }
                }
            }
        }
    }

    fn serialize_and_error<S>(result: anyhow::Result<S>) -> Result<NvList, Errno>
    where
        S: Serialize,
    {
        match result {
            Ok(phys) => match nvpair::to_nvlist(&phys) {
                Ok(nvl) => Ok(nvl),
                Err(e @ nvpair::Error::IoError(_)) => {
                    error!("Error in to_nvlist: {:?}", e);
                    Err(Errno::EIO)
                }
                Err(e @ nvpair::Error::UnknownNvPairType) => {
                    error!("Error in to_nvlist: {:?}", e);
                    Err(Errno::ENOTSUP)
                }
                Err(e) => {
                    error!("Error in to_nvlist: {:?}", e);
                    Err(Errno::EINVAL)
                }
            },
            Err(e) => {
                debug!("{:?}", e);
                Err(Errno::ENOENT)
            }
        }
    }

    pub fn get_pool_phys(&self, guid: PoolGuid) -> Result<NvList, Errno> {
        let object_access = self.object_access.as_ref().unwrap();
        let future =
            async move { Self::serialize_and_error(PoolPhys::get(object_access, guid).await) };
        self.runtime.block_on(future)
    }

    pub fn get_uberblock_phys(&self, guid: PoolGuid, txg: Txg) -> Result<NvList, Errno> {
        let object_access = self.object_access.as_ref().unwrap();
        let future = async move {
            Self::serialize_and_error(UberblockPhys::get(object_access, guid, txg).await)
        };
        self.runtime.block_on(future)
    }
}
