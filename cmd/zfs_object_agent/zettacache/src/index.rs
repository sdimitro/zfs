use crate::atime_histogram::AtimeHistogramPhys;
use crate::base_types::*;
use crate::block_access::*;
use crate::block_based_log::*;
use crate::extent_allocator::ExtentAllocator;
use crate::extent_allocator::ExtentAllocatorBuilder;
use futures::future;
use futures::StreamExt;
use futures_core::Stream;
use more_asserts::*;
use serde::{Deserialize, Serialize};
use std::cmp::max;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct IndexKey {
    pub guid: PoolGuid,
    pub block: BlockId,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
#[repr(packed)]
pub struct IndexValue {
    location: Option<DiskLocation>,
    // XXX remove this and figure out based on which slab it's in?  However,
    // currently we need to return the right buffer size to the kernel, and it
    // isn't passing us the expected read size.  So we need to change some
    // interfaces to make that work right.
    size: u32,
    atime: Atime,
}

impl IndexValue {
    pub fn new(location: Option<DiskLocation>, size: u32, atime: Atime) -> Self {
        Self {
            location,
            size,
            atime,
        }
    }
    pub fn extent(&self) -> Option<Extent> {
        self.location.map(|location| Extent {
            location,
            size: u64::from(self.size),
        })
    }
    pub fn size(&self) -> u32 {
        self.size
    }
    pub fn atime(&self) -> Atime {
        self.atime
    }
    pub fn location(&self) -> Option<DiskLocation> {
        self.location
    }
    pub fn set_location(&mut self, location: Option<DiskLocation>) {
        self.location = location;
    }
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub struct IndexEntry {
    pub key: IndexKey,
    pub value: IndexValue,
}
impl BlockBasedLogEntry for IndexEntry {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IndexRunPhys {
    // Note, entries at and before trim_key may exist on disk but are not considered
    // part of this index.  Notably, the atime_histogram does not include their
    // space.
    trim_key: Option<IndexKey>,

    last_key: Option<IndexKey>,
    atime_histogram: AtimeHistogramPhys,
    log: SummarizedBlockBasedLogPhys<IndexEntry>,
}

impl IndexRunPhys {
    pub fn new(first_ghost_atime: Atime, first_live_atime: Atime) -> Self {
        Self {
            trim_key: None,
            last_key: None,
            atime_histogram: AtimeHistogramPhys::new(first_ghost_atime, first_live_atime),
            log: Default::default(),
        }
    }

    pub fn claim(&self, builder: &mut ExtentAllocatorBuilder) {
        self.log.claim(builder);
    }

    pub fn iter_entries(&self, block_access: Arc<BlockAccess>) -> impl Stream<Item = IndexEntry> {
        self.log.iter_entries(block_access)
    }

    pub fn iter_log_chunks(
        &self,
        block_access: Arc<BlockAccess>,
    ) -> impl Stream<Item = BlockBasedLogChunk<IndexEntry>> {
        self.log.iter_chunks(block_access)
    }

    pub fn iter_log_summary(
        &self,
        block_access: Arc<BlockAccess>,
    ) -> impl Stream<Item = BlockBasedLogChunk<BlockBasedLogChunkSummaryEntry<IndexEntry>>> {
        self.log.iter_summary_chunks(block_access)
    }

    pub fn log_bytes(&self) -> u64 {
        self.log.bytes()
    }

    pub fn log_capacity_bytes(&self) -> u64 {
        self.log.capacity_bytes()
    }

    pub fn atime_histogram(&self) -> &AtimeHistogramPhys {
        &self.atime_histogram
    }

    pub fn last_key(&self) -> Option<IndexKey> {
        self.last_key
    }

    pub async fn verify_histogram(&self, block_access: Arc<BlockAccess>) {
        let mut histogram = AtimeHistogramPhys::new(
            self.atime_histogram.first_ghost(),
            self.atime_histogram.first_live(),
        );
        self.iter_entries(block_access)
            .for_each(|entry| {
                histogram.insert(entry.value);
                future::ready(())
            })
            .await;
        histogram.assert_eq(&self.atime_histogram);
        println!("Verified index histogram: {}", histogram);
    }
}

pub struct IndexRun {
    trim_key: Option<IndexKey>, // The key and all before it are logically removed from the index.
    last_key: Option<IndexKey>,
    atime_histogram: AtimeHistogramPhys,
    log: SummarizedBlockBasedLog<IndexEntry>,
}

impl std::fmt::Debug for IndexRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZettaCacheIndex")
            .field("last_key", &self.last_key)
            .finish()
    }
}

#[derive(Debug)]
pub struct IndexFlushDelta(SummarizedBlockBasedLogFlushDelta<IndexEntry>);

impl IndexRun {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: IndexRunPhys,
    ) -> Self {
        let index = Self {
            trim_key: phys.trim_key,
            last_key: phys.last_key,
            atime_histogram: phys.atime_histogram,
            log: SummarizedBlockBasedLog::open(block_access, extent_allocator, phys.log).await,
        };
        index
    }

    /// Returns new Phys and a Vec which can be passed to ReadOnlyIndexRun::update()
    pub async fn flush(&mut self) -> (IndexRunPhys, IndexFlushDelta) {
        let (log, new_chunks) = self.log.flush().await;
        (
            IndexRunPhys {
                trim_key: self.trim_key,
                last_key: self.last_key,
                atime_histogram: self.atime_histogram.clone(),
                log,
            },
            IndexFlushDelta(new_chunks),
        )
    }

    /// Retrieve the index phys. This only works if there are no pending log entries.
    /// Use flush() to retrieve the phys when there are pending entries.
    pub fn get_phys(&self) -> IndexRunPhys {
        IndexRunPhys {
            trim_key: self.trim_key,
            last_key: self.last_key,
            atime_histogram: self.atime_histogram.clone(),
            log: self.log.get_phys(),
        }
    }

    pub fn atime_histogram(&self) -> &AtimeHistogramPhys {
        &self.atime_histogram
    }

    pub fn first_ghost_atime(&self) -> Atime {
        self.atime_histogram.first_ghost()
    }

    pub fn first_live_atime(&self) -> Atime {
        self.atime_histogram.first_live()
    }

    pub fn update_last_key(&mut self, key: IndexKey) {
        if let Some(last_key) = self.last_key {
            assert_gt!(key, last_key);
        }
        self.last_key = Some(key);
    }

    pub fn append(&mut self, entry: IndexEntry) {
        self.update_last_key(entry.key);
        self.atime_histogram.insert(entry.value);
        self.log.append(entry);
    }

    pub fn clear(&mut self) {
        self.last_key = None;
        self.atime_histogram.clear();
        self.log.clear();
    }

    // Logically remove entries at and before `first`, which must be >= the current
    // `trim_key`.  The newly-obsoleted entries must have the provided
    // histogram.
    pub fn trim(&mut self, trim_key: IndexKey, obsoleted: &AtimeHistogramPhys) {
        if let Some(old_trim_key) = self.trim_key {
            assert_ge!(trim_key, old_trim_key);
        }

        self.last_key = Some(
            self.last_key
                .map(|last_key| max(trim_key, last_key))
                .unwrap_or(trim_key),
        );
        self.trim_key = Some(trim_key);
        self.atime_histogram -= obsoleted;
    }

    pub fn len(&self) -> u64 {
        self.log.len()
    }

    pub fn num_bytes(&self) -> u64 {
        self.log.num_bytes()
    }

    pub fn iter(&self) -> impl Stream<Item = IndexEntry> {
        self.log.iter()
    }

    pub fn trim_key(&self) -> Option<IndexKey> {
        self.trim_key
    }

    pub fn last_key(&self) -> Option<IndexKey> {
        self.last_key
    }

    pub async fn lookup(&self, key: IndexKey) -> Option<BlockBasedLogValueGuard<'_, IndexEntry>> {
        if let Some(trim_key) = self.trim_key {
            assert_gt!(key, trim_key);
        }
        self.log.lookup_by_key(&key, |entry| entry.key).await
    }
}

pub struct ReadOnlyIndexRun {
    trim_key: Option<IndexKey>,
    last_key: Option<IndexKey>,
    log: ReadOnlySummarizedBlockBasedLog<IndexEntry>,
}

impl ReadOnlyIndexRun {
    pub async fn open(block_access: Arc<BlockAccess>, phys: IndexRunPhys) -> Self {
        let index = Self {
            trim_key: phys.trim_key,
            last_key: phys.last_key,
            log: ReadOnlySummarizedBlockBasedLog::open(block_access, phys.log).await,
        };
        index
    }

    pub fn last_key(&self) -> Option<IndexKey> {
        self.last_key
    }

    pub async fn lookup(&self, key: IndexKey) -> Option<BlockBasedLogValueGuard<'_, IndexEntry>> {
        if let Some(trim_key) = self.trim_key {
            assert_gt!(key, trim_key);
        }
        self.log.lookup_by_key(&key, |entry| entry.key).await
    }

    /// Update this readonly view to reflect newly-appended chunks.
    pub fn update(&mut self, phys: IndexRunPhys, new_chunks: &IndexFlushDelta) {
        self.trim_key = phys.trim_key;
        self.last_key = phys.last_key;
        self.log.update(phys.log, &new_chunks.0);
    }
}
