use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::info;
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use util::From64;

use crate::base_types::DiskId;
use crate::block_access::BlockAccess;
use crate::superblock::SuperblockPhys;
use crate::superblock::SUPERBLOCK_SIZE;

pub enum CacheOpenMode {
    DeviceList(Vec<PathBuf>),
    DiscoveryDirectory(PathBuf),
}

impl CacheOpenMode {
    pub async fn device_paths(self) -> Result<Vec<PathBuf>> {
        Ok(match self {
            CacheOpenMode::DeviceList(paths) => paths,
            CacheOpenMode::DiscoveryDirectory(dir) => discover_devices(&dir).await?,
        })
    }
}

#[derive(Debug)]
struct DiscoveredDevice {
    device_path: PathBuf,
    superblock: SuperblockPhys,
}

impl DiscoveredDevice {
    fn new(device_path: PathBuf, superblock: SuperblockPhys) -> Self {
        DiscoveredDevice {
            device_path,
            superblock,
        }
    }

    async fn from_path(path: PathBuf) -> Result<Self> {
        let mut file = File::open(&path)
            .await
            .with_context(|| format!("discovery: open {path:?}"))?;
        let mut buf = vec![0u8; usize::from64(SUPERBLOCK_SIZE)];
        file.read_exact(&mut buf)
            .await
            .with_context(|| format!("discovery: read_exact {path:?}"))?;
        let (superblock, _) = BlockAccess::chunk_from_raw_impl::<SuperblockPhys>(&buf)
            .with_context(|| format!("discovery: parse label {path:?}"))?;
        Ok(DiscoveredDevice::new(path, superblock))
    }
}

async fn discover_devices(dir_path: &Path) -> Result<Vec<PathBuf>> {
    let mut caches = HashMap::<u64, BTreeMap<DiskId, DiscoveredDevice>>::new();

    let mut discovery = FuturesUnordered::new();
    let mut dir = fs::read_dir(dir_path).await?;
    while let Some(entry) = dir.next_entry().await? {
        if entry.metadata().await?.is_dir() {
            continue;
        }
        discovery.push(DiscoveredDevice::from_path(entry.path()))
    }
    while let Some(result) = discovery.next().await {
        match result {
            Ok(device) => {
                let cache_guid = device.superblock.guid;
                let cache = caches.entry(cache_guid).or_default();
                if let Some(old_device) = cache.insert(device.superblock.disk, device) {
                    // If we crash in the middle of a zcache-add for one device
                    // and then do zcache-add for another device, we may get
                    // into a situation where discovery runs into two devices
                    // with the same DiskID and cache GUID. Until we make cache
                    // device import more deterministic (see DLPX-81000) error
                    // out.
                    return Err(anyhow!(
                        "found two disks with {:?} for cache {cache_guid}",
                        old_device.superblock.disk
                    ));
                }
            }
            Err(why) => info!("{why}"),
        };
    }
    filter_invalid_caches(&mut caches);
    match caches.values().next() {
        Some(cache) => {
            if caches.len() > 1 {
                // XXX - In the future we probably want to be able to specify a
                // cache by GUID so we can get past this error.
                Err(anyhow!(
                    "multiple valid caches found in {dir_path:?}: {:?}",
                    caches.keys().collect::<Vec<_>>(),
                ))
            } else {
                Ok(cache
                    .iter()
                    .map(|(_, dev)| dev.device_path.clone())
                    .collect())
            }
        }
        None => Err(anyhow!("no valid caches found in {dir_path:?}")),
    }
}

fn filter_invalid_caches(caches: &mut HashMap<u64, BTreeMap<DiskId, DiscoveredDevice>>) {
    // Only retain caches that have a primary block
    caches.retain(|_, disks| disks.values().any(|disk| disk.superblock.primary.is_some()));

    // Only retain caches whose devices we've discovered
    caches.retain(|cache_guid, disks| {
        let mut disk_ids_from_discovery = HashSet::new();
        let mut disk_ids_from_primary = HashSet::new();
        for (&disk_id, disk) in disks {
            disk_ids_from_discovery.insert(disk_id);
            if let Some(primary) = &disk.superblock.primary {
                disk_ids_from_primary = primary.disks.keys().copied().collect();
            }
        }
        assert!(!disk_ids_from_primary.is_empty());
        disk_ids_from_primary
            .difference(&disk_ids_from_discovery)
            .inspect(|id| info!("cache {cache_guid} can't find {id:?}"))
            .count()
            == 0
    });
}
