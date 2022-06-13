use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::*;
use tokio::fs;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use util::message::SUPERBLOCK_SIZE;

use crate::base_types::CacheGuid;
use crate::base_types::DiskGuid;
use crate::base_types::DiskId;
use crate::block_access::BlockAccess;
use crate::superblock::SuperblockPhys;

#[derive(Debug, Clone)]
pub enum CacheOpenMode {
    DeviceList(Vec<PathBuf>),
    DiscoveryDirectory(PathBuf, Option<CacheGuid>),
    None, // no zettacache
}

impl CacheOpenMode {
    pub async fn device_paths(self) -> Result<Vec<PathBuf>> {
        Ok(match self {
            CacheOpenMode::DeviceList(paths) => paths,
            CacheOpenMode::DiscoveryDirectory(dir, target_guid) => {
                discover_devices(&dir, target_guid).await?
            }
            CacheOpenMode::None => Default::default(),
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
            .with_context(|| format!("open {path:?}"))?;

        let filetype = file.metadata().await?.file_type();
        if !filetype.is_block_device() && !filetype.is_file() {
            return Err(anyhow!("{path:?} is not a file nor a block device"));
        }

        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        file.read_exact(&mut buf)
            .await
            .with_context(|| format!("read_exact {path:?}"))?;
        let (superblock, _) = BlockAccess::chunk_from_raw_impl::<SuperblockPhys>(&buf)
            .with_context(|| format!("parse label {path:?}"))?;
        Ok(DiscoveredDevice::new(path, superblock))
    }
}

async fn discover_devices(dir_path: &Path, target_guid: Option<CacheGuid>) -> Result<Vec<PathBuf>> {
    let mut caches = HashMap::<_, BTreeMap<_, _>>::new();

    let mut discovery = FuturesUnordered::new();
    let mut canonical_entries = HashSet::new();
    let mut dir = fs::read_dir(dir_path).await?;
    while let Some(entry) = dir.next_entry().await? {
        match entry.metadata().await {
            Ok(meta) => {
                if meta.is_dir() {
                    continue;
                }
            }
            Err(why) => {
                debug!("discovery: {entry:?}.metadata() failed. error: {why:?}");
                continue;
            }
        }

        // In certain directories under /dev we've come across device symlinks
        // that resolve to the same device (e.g. /dev/disk/by-id on AWS). In
        // order to avoid trying to open the same device twice, we always
        // resolve device symlinks and skip the ones whose device files we've
        // encountered already.
        let path = entry.path();
        match fs::canonicalize(&path).await {
            Ok(canonical_path) => {
                if !canonical_entries.contains(&canonical_path) {
                    canonical_entries.insert(canonical_path);

                    // We use the original directory path here instead of the
                    // resolved/canonical path to avoid surprising the user. E.g. `zcache
                    // list -f` should show device paths from the discovery directory
                    // supplied by the user instead of the resolved device paths.
                    discovery.push(DiscoveredDevice::from_path(path))
                }
            }
            Err(why) => {
                debug!("discovery: fs::canonicalize({path:?}) failed. error: {why:?}");
                continue;
            }
        }
    }

    while let Some(result) = discovery.next().await {
        match result {
            Ok(device) => {
                debug!("discovery: found device: {device:?}");
                let cache_guid = device.superblock.cache_guid;
                let cache = caches.entry(cache_guid).or_default();
                let disk_guid = device.superblock.disk_guid;
                if let Some(old_device) = cache.insert((device.superblock.disk, disk_guid), device)
                {
                    return Err(anyhow!(
                        "found two disks with {:?} for {cache_guid:?} with {disk_guid:?}",
                        old_device.superblock.disk,
                    ));
                }
            }
            Err(why) => debug!("discovery: error: {why:?}"),
        };
    }

    caches.retain(|&guid, disks| is_valid_cache(guid, disks));
    if let Some(guid) = target_guid {
        caches.retain(|cache_guid, _| *cache_guid == guid);
    }

    match caches.values().next() {
        Some(cache) => {
            if caches.len() > 1 {
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

fn is_valid_cache(
    cache_guid: CacheGuid,
    disks: &mut BTreeMap<(DiskId, Option<DiskGuid>), DiscoveredDevice>,
) -> bool {
    let primary = disks
        .values()
        .find(|&disk| disk.superblock.primary.is_some());

    match primary {
        Some(disk) => {
            let disks_from_primary = disk
                .superblock
                .primary
                .as_ref()
                .unwrap()
                .disks
                .iter()
                .map(|(k, v)| (*k, v.guid))
                .collect::<HashMap<_, _>>();

            // Only retain disks with IDs that are part of the primary block
            disks.retain(
                |(id, disk_guid), _| match (disks_from_primary.get(id), *disk_guid) {
                    (Some(&guid_from_primary), Some(guid_from_superblock)) => {
                        guid_from_superblock == guid_from_primary
                    }
                    (Some(_guid_from_primary), None) => true,
                    (None, _) => false,
                },
            );

            // If we are missing any device mentioned in the primary block,
            // then this cache is invalid
            let disks_ids_from_discovery = disks
                .keys()
                .map(|(id, _)| id)
                .copied()
                .collect::<HashSet<_>>();
            let disk_ids_from_primary = disks_from_primary.keys().copied().collect::<HashSet<_>>();
            disk_ids_from_primary
                .difference(&disks_ids_from_discovery)
                .inspect(|id| info!("cache {cache_guid:?} can't find {id:?}"))
                .count()
                == 0
        }
        None => false,
    }
}
