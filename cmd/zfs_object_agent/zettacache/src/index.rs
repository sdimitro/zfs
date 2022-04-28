use std::cmp::max;
use std::mem::size_of;
use std::num::NonZeroU64;
use std::sync::Arc;

use futures::future;
use futures::StreamExt;
use futures_core::Stream;
use more_asserts::*;
use safer_ffi::prelude::*;
use serde::de::Error;
use serde::de::Visitor;
use serde::Deserialize;
use serde::Serialize;
use util::message::slice_to_struct;
use util::message::struct_to_slice;
use util::writeln_stdout;

use crate::atime_histogram::AtimeHistogramPhys;
use crate::base_types::*;
use crate::block_access::*;
use crate::block_based_log::summarized::*;
use crate::block_based_log::*;
use crate::pool_id::PoolId;
use crate::slab_allocator::SlabAccess;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
#[repr(packed)]
pub struct IndexKey {
    id: PoolId,
    block: BlockId,
}

impl IndexKey {
    pub fn new(id: PoolId, block: BlockId) -> Self {
        Self { id, block }
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq)]
#[repr(packed)]
pub struct IndexValue {
    location: Option<DiskLocation>,
    sectors: u16,
    atime: Atime,
}

impl IndexValue {
    const SECTOR_SHIFT: usize = 9;
    pub fn new(location: Option<DiskLocation>, size: u32, atime: Atime) -> Self {
        assert_eq!(size % (1 << Self::SECTOR_SHIFT), 0);
        Self {
            location,
            sectors: (size >> Self::SECTOR_SHIFT).try_into().unwrap(),
            atime,
        }
    }
    pub fn extent(&self) -> Option<Extent> {
        self.location.map(|location| Extent {
            location,
            size: u64::from(self.size()),
        })
    }
    pub fn size(&self) -> u32 {
        u32::from(self.sectors) << Self::SECTOR_SHIFT
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

#[derive(Debug, Copy, Clone)]
pub struct IndexEntry {
    pub key: IndexKey,
    pub value: IndexValue,
}
impl BlockBasedLogEntry for IndexEntry {}
impl SummarizedBlockBasedLogEntry for IndexEntry {
    type Key = IndexKey;
    fn key(&self) -> Self::Key {
        self.key
    }
}
impl From<&IndexEntryPhys> for IndexEntry {
    fn from(phys: &IndexEntryPhys) -> Self {
        IndexEntry {
            key: IndexKey {
                id: PoolId(phys.pool_id),
                block: BlockId(phys.block),
            },
            value: IndexValue {
                location: NonZeroU64::new(phys.location).map(DiskLocation::from_raw),
                sectors: phys.sectors,
                atime: Atime(phys.atime),
            },
        }
    }
}
impl Serialize for IndexEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let phys: IndexEntryPhys = self.into();
        if cfg!(target_endian = "little") {
            serializer.serialize_bytes(struct_to_slice(&phys))
        } else {
            panic!("little endian machine required");
        }
    }
}
impl<'de> Deserialize<'de> for IndexEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let visitor = IndexEntryVisitor;
        deserializer.deserialize_bytes(visitor)
    }
}
struct IndexEntryVisitor;
impl<'de> Visitor<'de> for IndexEntryVisitor {
    type Value = IndexEntry;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            formatter,
            "a byte array of length {}",
            size_of::<IndexEntryPhys>()
        )
    }
    fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        if cfg!(target_endian = "little") {
            // slice_to_struct() copies the data, but we should be able to just get
            // a pointer, because it's packed (alignment unconstrained).  However,
            // this is not significant to performance.
            let phys: IndexEntryPhys = slice_to_struct(v);
            Ok((&phys).into())
        } else {
            panic!("little endian machine required");
        }
    }
}

#[derive_ReprC]
#[repr(C)]
#[repr(packed)]
struct IndexEntryPhys {
    pool_id: u8,
    block: u64,
    location: u64, // if zero then None
    sectors: u16,
    atime: u32,
}
impl From<&IndexEntry> for IndexEntryPhys {
    fn from(entry: &IndexEntry) -> Self {
        IndexEntryPhys {
            pool_id: entry.key.id.0,
            block: entry.key.block.0,
            location: entry
                .value
                .location
                .as_ref()
                .map(|l| l.to_raw().get())
                .unwrap_or_default(),
            sectors: entry.value.sectors,
            atime: entry.value.atime.0,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct IndexRunPhys {
    // Note, entries at and before trim_key may exist on disk but are not considered
    // part of this index.  Notably, the atime_histogram does not include their
    // space.
    trim_key: Option<IndexKey>,

    last_key: Option<IndexKey>,
    atime_histogram_phys: AtimeHistogramPhys,
    log: SummarizedBlockBasedLogPhys<IndexEntry>,
}

impl IndexRunPhys {
    pub fn new(first_ghost_atime: Atime, first_live_atime: Atime) -> Self {
        Self {
            trim_key: None,
            last_key: None,
            atime_histogram_phys: AtimeHistogramPhys::new(first_ghost_atime, first_live_atime),
            log: Default::default(),
        }
    }

    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.log.claim(builder);
    }

    pub fn iter(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = IndexEntry> {
        self.log.iter(block_access, slab_access)
    }

    pub fn iter_chunks(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = BlockBasedLogChunk<IndexEntry>> {
        self.log.iter_chunks(block_access, slab_access)
    }

    pub fn iter_summary_chunks(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = BlockBasedLogChunk<BlockBasedLogChunkSummaryEntry<IndexEntry>>> {
        self.log.iter_summary_chunks(block_access, slab_access)
    }

    pub fn log_bytes(&self) -> u64 {
        self.log.bytes()
    }

    pub fn log_capacity_bytes(&self, slab_access: &SlabAccess) -> u64 {
        self.log.capacity_bytes(slab_access)
    }

    pub fn atime_histogram(&self) -> &AtimeHistogramPhys {
        &self.atime_histogram_phys
    }

    pub fn last_key(&self) -> Option<IndexKey> {
        self.last_key
    }

    pub async fn verify_histogram(&self, block_access: Arc<BlockAccess>, slab_access: &SlabAccess) {
        let mut histogram = AtimeHistogramPhys::new(
            self.atime_histogram_phys.first_ghost(),
            self.atime_histogram_phys.first_live(),
        );
        self.iter(block_access, slab_access)
            .for_each(|entry| {
                histogram.insert(entry.value);
                future::ready(())
            })
            .await;
        histogram.assert_eq(&self.atime_histogram_phys);
        writeln_stdout!("Verified index histogram: {}", histogram);
    }
}

pub struct IndexRun {
    trim_key: Option<IndexKey>, // The key and all before it are logically removed from the index.
    last_key: Option<IndexKey>,
    atime_histogram_phys: AtimeHistogramPhys,
    log: SummarizedBlockBasedLog<IndexEntry>,
}

#[derive(Debug)]
pub struct IndexFlushDelta(SummarizedBlockBasedLogFlushDelta<IndexEntry>);

impl IndexFlushDelta {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl IndexRun {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        slab_allocator: Arc<SlabAllocator>,
        phys: IndexRunPhys,
    ) -> Self {
        let index = Self {
            trim_key: phys.trim_key,
            last_key: phys.last_key,
            atime_histogram_phys: phys.atime_histogram_phys,
            log: SummarizedBlockBasedLog::open(block_access, slab_allocator, phys.log).await,
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
                atime_histogram_phys: self.atime_histogram_phys.clone(),
                log,
            },
            IndexFlushDelta(new_chunks),
        )
    }

    pub fn atime_histogram(&self) -> &AtimeHistogramPhys {
        &self.atime_histogram_phys
    }

    pub fn first_ghost_atime(&self) -> Atime {
        self.atime_histogram_phys.first_ghost()
    }

    pub fn first_live_atime(&self) -> Atime {
        self.atime_histogram_phys.first_live()
    }

    pub fn update_last_key(&mut self, key: IndexKey) {
        if let Some(last_key) = self.last_key {
            assert_ge!(key, last_key);
        }
        self.last_key = Some(key);
    }

    pub fn append(&mut self, list: Vec<IndexEntry>) {
        if let Some(last_entry) = list.last() {
            self.update_last_key(last_entry.key);
        }
        for entry in &list {
            self.atime_histogram_phys.insert(entry.value);
        }
        self.log.append(list);
    }

    pub fn clear(&mut self) {
        self.last_key = None;
        self.atime_histogram_phys.clear();
        self.log.clear();
    }

    // Logically remove entries at and before `trim_key`, which must be >= the current
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
        self.atime_histogram_phys -= obsoleted;
        self.log.trim(trim_key);
    }

    pub fn len(&self) -> u64 {
        self.log.len()
    }

    pub fn num_bytes(&self) -> u64 {
        self.log.num_bytes()
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> impl Stream<Item = IndexEntry> {
        self.log.iter()
    }

    pub fn iter_chunks(&self) -> impl Stream<Item = BlockBasedLogChunk<IndexEntry>> {
        self.log.iter_chunks()
    }

    pub fn trim_key(&self) -> Option<IndexKey> {
        self.trim_key
    }

    pub fn last_key(&self) -> Option<IndexKey> {
        self.last_key
    }

    /// Returns (value, chunk_cache_hit), where the value is the value corresponding
    /// to the key argument if found, and chunk_cache_hit that tells us whether we found
    /// the value on the chunk cache (true) or had to reach out to disk (false).
    pub async fn lookup(
        &self,
        key: IndexKey,
    ) -> (Option<BlockBasedLogValueGuard<'_, IndexEntry>>, bool) {
        if let Some(trim_key) = self.trim_key {
            assert_gt!(key, trim_key);
        }
        self.log.lookup_by_key(&key).await
    }
}

pub struct ReadOnlyIndexRun {
    trim_key: Option<IndexKey>,
    last_key: Option<IndexKey>,
    log: ReadOnlySummarizedBlockBasedLog<IndexEntry>,
}

impl ReadOnlyIndexRun {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        slab_allocator: Arc<SlabAllocator>,
        phys: IndexRunPhys,
    ) -> Self {
        let index = Self {
            trim_key: phys.trim_key,
            last_key: phys.last_key,
            log: ReadOnlySummarizedBlockBasedLog::open(block_access, slab_allocator, phys.log)
                .await,
        };
        index
    }

    pub fn last_key(&self) -> Option<IndexKey> {
        self.last_key
    }

    /// Returns (value, chunk_cache_hit), where the value is the value corresponding
    /// to the key argument if found, and chunk_cache_hit that tells us whether we found
    /// the value on the chunk cache (true) or had to reach out to disk (false).
    pub async fn lookup(
        &self,
        key: IndexKey,
    ) -> (Option<BlockBasedLogValueGuard<'_, IndexEntry>>, bool) {
        if let Some(trim_key) = self.trim_key {
            assert_gt!(key, trim_key);
        }
        self.log.lookup_by_key(&key).await
    }

    /// Update this readonly view to reflect newly-appended chunks.
    pub fn update(&mut self, phys: IndexRunPhys, new_chunks: &IndexFlushDelta) {
        self.trim_key = phys.trim_key;
        self.last_key = phys.last_key;
        self.log.update(phys.log, &new_chunks.0);
    }
}
