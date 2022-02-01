use crate::base_types::*;
use crate::block_access::BlockAccess;
use crate::block_access::EncodeType;
use crate::extent_allocator::ExtentAllocator;
use crate::extent_allocator::ExtentAllocatorBuilder;
use anyhow::Context;
use async_stream::stream;
use futures::future::join;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use futures_core::Stream;
use lazy_static::lazy_static;
use log::*;
use lru::LruCache;
use more_asserts::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::mem;
use std::ops::Add;
use std::ops::Bound::*;
use std::ops::Sub;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;
use util::get_tunable;
use util::nice_p2size;
use util::super_trace;
use util::with_alloctag;
use util::zettacache_stats::DiskIoType;
use util::AlignedVec;
use util::From64;
use util::LockSet;

lazy_static! {
    static ref ENTRIES_PER_CHUNK: usize = get_tunable("entries_per_chunk", 200);
    // Note: kernel sends writes to disk in at most 256K chunks (at least with nvme driver)
    static ref WRITE_AGGREGATION_SIZE: usize = get_tunable("write_aggregation_size", 256 * 1024);
    // We primarily use the chunk cache to ensure that when looking up all the
    // entries in an object, we need at most one read from the index.  So we
    // only need as many chunks in the cache as the number of objects that we
    // might be processing concurrently.
    static ref CHUNK_CACHE_ENTRIES: usize = get_tunable("chunk_cache_entries", 128);
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BlockBasedLogPhys<T: BlockBasedLogEntry> {
    // XXX on-disk format could just be array of extents; offset can be derived
    // from size of previous extents. We do need the btree in RAM though so that
    // we can do random reads on the Index (unless the ChunkSummary points
    // directly to the on-disk location)
    extents: BTreeMap<LogOffset, Extent>, // offset -> disk_location, size
    next_chunk: ChunkId,
    next_chunk_offset: LogOffset, // logical byte offset of next chunk to write
    num_entries: u64,
    entry_type: PhantomData<T>,
}

// Unfortunately, #[derive(Default)] doesn't generate exactly this code; it
// requires that T: Default, which is not the case, and is not necessary here.
// See https://github.com/rust-lang/rust/issues/26925
impl<T: BlockBasedLogEntry> Default for BlockBasedLogPhys<T> {
    fn default() -> Self {
        Self {
            extents: Default::default(),
            next_chunk: Default::default(),
            next_chunk_offset: Default::default(),
            num_entries: Default::default(),
            entry_type: PhantomData,
        }
    }
}

impl<T: BlockBasedLogEntry> BlockBasedLogPhys<T> {
    pub fn clear(&mut self, extent_allocator: &ExtentAllocator) {
        for extent in self.extents.values() {
            extent_allocator.free(extent);
        }
        *self = Default::default();
    }

    pub fn claim(&self, builder: &mut ExtentAllocatorBuilder) {
        for extent in self.extents.values() {
            builder.claim(extent);
        }
    }

    // Since &self is not captured by the returned Stream (its extent list is
    // cloned), callers must ensure that the disk space represented by the
    // extents is not overwritten before the stream terminates.  i.e. do not
    // call .clear().
    pub fn iter_chunks(
        &self,
        block_access: Arc<BlockAccess>,
    ) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        // XXX is it possible to do this without copying self.phys.extents?  Not
        // a huge deal I guess since it should be small.
        let extents = self.extents.clone();
        let next_chunk = self.next_chunk;
        let next_chunk_offset = self.next_chunk_offset;

        stream! {
            let mut chunk_id = ChunkId(0);
            for (offset, extent) in extents.iter() {
                // XXX Probably want to do smaller i/os than the entire extent
                // (which is up to 128MB).  Also want to issue a few in
                // parallel?

                let truncated_extent =
                    extent.range(0, min(extent.size, next_chunk_offset - *offset));
                let extent_bytes = block_access.read_raw(truncated_extent, DiskIoType::MaintenanceRead).await;
                let mut total_consumed = 0;
                while total_consumed < extent_bytes.len() {
                    let chunk_location = extent.location.offset() + total_consumed as u64;
                    super_trace!("decoding {:?} from {:?}", chunk_id, chunk_location);
                    // XXX handle checksum error here
                    let (chunk, consumed): (BlockBasedLogChunk<T>, usize) =
                        with_alloctag("BlockBasedLogPhys::iter_chunks()", || {
                            block_access
                                .chunk_from_raw(&extent_bytes[total_consumed..])
                                .with_context(|| format!("{:?} at {:?}", chunk_id, chunk_location))
                                .unwrap()
                        });
                    assert_eq!(chunk.id, chunk_id);
                    yield chunk;
                    chunk_id = chunk_id.next();
                    total_consumed += consumed;
                    if chunk_id == next_chunk {
                        break;
                    }
                }
            }
        }
    }

    pub fn iter_entries(&self, block_access: Arc<BlockAccess>) -> impl Stream<Item = T> {
        self.iter_chunks(block_access)
            .flat_map(|chunk| stream::iter(chunk.entries.into_iter()))
    }

    pub fn bytes(&self) -> u64 {
        self.next_chunk_offset.0
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.extents.values().map(|x| x.size).sum()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SummarizedBlockBasedLogPhys<T: BlockBasedLogEntry> {
    #[serde(bound(deserialize = "T: DeserializeOwned"))]
    this: BlockBasedLogPhys<T>,
    #[serde(bound(deserialize = "T: DeserializeOwned"))]
    chunk_summary: BlockBasedLogPhys<BlockBasedLogChunkSummaryEntry<T>>,
}

impl<T: BlockBasedLogEntry> Default for SummarizedBlockBasedLogPhys<T> {
    fn default() -> Self {
        Self {
            this: Default::default(),
            chunk_summary: Default::default(),
        }
    }
}

impl<T: BlockBasedLogEntry> SummarizedBlockBasedLogPhys<T> {
    pub fn claim(&self, builder: &mut ExtentAllocatorBuilder) {
        self.this.claim(builder);
        self.chunk_summary.claim(builder);
    }

    pub fn iter_entries(&self, block_access: Arc<BlockAccess>) -> impl Stream<Item = T> {
        self.this.iter_entries(block_access)
    }

    pub fn iter_chunks(
        &self,
        block_access: Arc<BlockAccess>,
    ) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        self.this.iter_chunks(block_access)
    }

    pub fn iter_summary_chunks(
        &self,
        block_access: Arc<BlockAccess>,
    ) -> impl Stream<Item = BlockBasedLogChunk<BlockBasedLogChunkSummaryEntry<T>>> {
        self.chunk_summary.iter_chunks(block_access)
    }

    pub fn bytes(&self) -> u64 {
        self.chunk_summary.bytes() + self.this.bytes()
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.chunk_summary.capacity_bytes() + self.this.capacity_bytes()
    }
}

pub trait BlockBasedLogEntry:
    'static + Serialize + DeserializeOwned + Copy + Clone + Unpin + Send + Sync
{
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub struct BlockBasedLogChunkSummaryEntry<T: BlockBasedLogEntry> {
    offset: LogOffset,
    #[serde(bound(deserialize = "T: DeserializeOwned"))]
    first_entry: T,
}
impl<T: BlockBasedLogEntry> OnDisk for BlockBasedLogChunkSummaryEntry<T> {}
impl<T: BlockBasedLogEntry> BlockBasedLogEntry for BlockBasedLogChunkSummaryEntry<T> {}

pub struct BlockBasedLog<T: BlockBasedLogEntry> {
    block_access: Arc<BlockAccess>,
    extent_allocator: Arc<ExtentAllocator>,
    phys: BlockBasedLogPhys<T>,
    pending_entries: Vec<T>,
}

pub struct SummarizedBlockBasedLog<T: BlockBasedLogEntry> {
    readonly: ReadOnlySummarizedBlockBasedLog<T>,
    this: BlockBasedLog<T>,
    chunk_summary: BlockBasedLog<BlockBasedLogChunkSummaryEntry<T>>,
}

pub struct ReadOnlySummarizedBlockBasedLog<T: BlockBasedLogEntry> {
    block_access: Arc<BlockAccess>,
    this: BlockBasedLogPhys<T>,
    chunk_summary: BlockBasedLogPhys<BlockBasedLogChunkSummaryEntry<T>>,
    chunks: Vec<BlockBasedLogChunkSummaryEntry<T>>,
    chunk_cache: Mutex<LruCache<ChunkId, BlockBasedLogChunk<T>>>,
    chunk_reads: LockSet<ChunkId>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BlockBasedLogChunk<T: BlockBasedLogEntry> {
    id: ChunkId,
    offset: LogOffset,
    #[serde(bound(deserialize = "Vec<T>: DeserializeOwned"))]
    entries: Vec<T>,
}

#[derive(Serialize, Debug)]
pub struct BlockBasedLogChunkBorrowed<'a, T: BlockBasedLogEntry> {
    id: ChunkId,
    offset: LogOffset,
    entries: &'a [T],
}

impl<T: BlockBasedLogEntry> BlockBasedLog<T> {
    pub fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: BlockBasedLogPhys<T>,
    ) -> BlockBasedLog<T> {
        BlockBasedLog {
            block_access,
            extent_allocator,
            phys,
            pending_entries: Vec::new(),
        }
    }

    pub async fn flush(&mut self) -> BlockBasedLogPhys<T> {
        self.flush_impl(|_, _, _| {}).await;
        self.phys.clone()
    }

    pub fn len(&self) -> u64 {
        self.phys.num_entries + self.pending_entries.len() as u64
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.phys.num_entries == 0 && self.pending_entries.is_empty()
    }

    /// Size of the on-disk representation
    pub fn num_bytes(&self) -> u64 {
        self.phys.next_chunk_offset.0
    }

    pub fn pending_len(&self) -> u64 {
        self.pending_entries.len() as u64
    }

    pub fn pending_bytes(&self) -> u64 {
        self.pending_len() * mem::size_of::<T>() as u64
    }

    pub fn append(&mut self, entry: T) {
        with_alloctag("BlockBasedLog.pending_entries", || {
            self.pending_entries.push(entry)
        });
        // XXX if too many pending, initiate flush?
    }

    async fn flush_impl<F>(&mut self, mut new_chunk_fn: F)
    where
        F: FnMut(ChunkId, LogOffset, T),
    {
        if self.pending_entries.is_empty() {
            return;
        }

        let writes_stream = FuturesUnordered::new();
        let mut pending_write: Option<(DiskLocation, AlignedVec)> = None;
        for pending_entries_chunk in self.pending_entries.chunks(*ENTRIES_PER_CHUNK) {
            let chunk = BlockBasedLogChunkBorrowed {
                id: self.phys.next_chunk,
                offset: self.phys.next_chunk_offset,
                entries: pending_entries_chunk,
            };

            let first_entry = *chunk.entries.first().unwrap();

            // XXX I think we only want to use Bincode for the main index?
            let raw_chunk = self
                .block_access
                .chunk_to_raw(EncodeType::BincodeFixint, &chunk);
            let raw_size = raw_chunk.len() as u64;
            let extent = match self.next_write_location() {
                Some(extent) if extent.size >= raw_size => extent,
                Some(extent) => {
                    // free the unused tail of this extent
                    self.extent_allocator.free(&extent);
                    if let Some((_, last_extent)) = self.phys.extents.iter_mut().next_back() {
                        assert!(last_extent.contains(&extent));
                        last_extent.size -= extent.size;
                    };

                    let extent = self.extent_allocator.allocate(raw_size);
                    self.phys
                        .extents
                        .insert(self.phys.next_chunk_offset, extent);
                    extent
                }
                None => {
                    let extent = self.extent_allocator.allocate(raw_size);
                    self.phys
                        .extents
                        .insert(self.phys.next_chunk_offset, extent);
                    extent
                }
            };
            assert_ge!(extent.size, raw_size);
            // XXX add name of this log for debug purposes?
            super_trace!(
                "flushing BlockBasedLog: writing {:?} ({:?}) with {} entries ({} bytes) to {:?}",
                chunk.id,
                chunk.offset,
                chunk.entries.len(),
                raw_chunk.len(),
                extent.location,
            );
            match pending_write {
                Some((pending_location, pending_vec))
                    if extent.location != pending_location + pending_vec.len()
                        || pending_vec.unused_capacity() < raw_chunk.len() =>
                {
                    writes_stream.push(self.block_access.write_raw(
                        pending_location,
                        pending_vec.into(),
                        DiskIoType::MaintenanceWrite,
                    ));
                    pending_write = None;
                }
                _ => (),
            }
            if pending_write.is_none() && raw_chunk.len() < 2 * *WRITE_AGGREGATION_SIZE {
                pending_write = Some((
                    extent.location,
                    with_alloctag("BlockBasedLog::flush_impl()", || {
                        AlignedVec::with_capacity(
                            *WRITE_AGGREGATION_SIZE,
                            self.block_access.round_up_to_sector(1),
                        )
                    }),
                ));
            }
            match &mut pending_write {
                Some((pending_location, pending_vec)) => {
                    assert_eq!(*pending_location + pending_vec.len(), extent.location);
                    pending_vec.extend_from_slice(&raw_chunk);
                }
                None => writes_stream.push(self.block_access.write_raw(
                    extent.location,
                    raw_chunk,
                    DiskIoType::MaintenanceWrite,
                )),
            }

            new_chunk_fn(chunk.id, chunk.offset, first_entry);

            self.phys.num_entries += chunk.entries.len() as u64;
            self.phys.next_chunk = self.phys.next_chunk.next();
            self.phys.next_chunk_offset.0 += raw_size;
        }
        if let Some((pending_location, pending_vec)) = pending_write {
            writes_stream.push(self.block_access.write_raw(
                pending_location,
                pending_vec.into(),
                DiskIoType::MaintenanceWrite,
            ));
        }
        writes_stream.for_each(|_| async move {}).await;
        self.pending_entries.truncate(0);
    }

    pub fn clear(&mut self) {
        self.pending_entries.clear();
        self.phys.clear(&self.extent_allocator);
    }

    fn next_write_location(&self) -> Option<Extent> {
        self.phys
            .extents
            .iter()
            .next_back()
            .map(|(&offset, extent)| {
                // There shouldn't be any extents after the last (partially-full) one.
                assert_ge!(self.phys.next_chunk_offset, offset);
                let offset_within_extent = self.phys.next_chunk_offset - offset;
                // The last extent should go at least to the end of the chunks.
                assert_le!(offset_within_extent, extent.size);
                extent.range(offset_within_extent, extent.size - offset_within_extent)
            })
    }

    /// Iterates the on-disk state; panics if there are pending changes.
    pub fn iter(&self) -> impl Stream<Item = T> {
        assert!(self.pending_entries.is_empty());
        self.phys.iter_entries(self.block_access.clone())
    }
}
impl<T: BlockBasedLogEntry> ReadOnlySummarizedBlockBasedLog<T> {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        phys: SummarizedBlockBasedLogPhys<T>,
    ) -> Self {
        // load in summary from disk
        let begin = Instant::now();
        // XXX how to measure memory usage, since it's gathered async?  Copy it later?  Or just rely on the log statement below?
        let chunks = phys
            .chunk_summary
            .iter_entries(block_access.clone())
            .collect::<Vec<_>>()
            .await;
        info!(
            "loaded summary of {} chunks ({}) in {}ms",
            chunks.len(),
            nice_p2size(phys.chunk_summary.bytes()),
            begin.elapsed().as_millis()
        );

        Self {
            block_access,
            this: phys.this,
            chunk_summary: phys.chunk_summary,
            chunks,
            chunk_cache: Mutex::new(LruCache::new(*CHUNK_CACHE_ENTRIES)),
            chunk_reads: Default::default(),
        }
    }

    pub fn get_phys(&self) -> SummarizedBlockBasedLogPhys<T> {
        SummarizedBlockBasedLogPhys {
            this: self.this.clone(),
            chunk_summary: self.chunk_summary.clone(),
        }
    }

    pub fn len(&self) -> u64 {
        self.this.num_entries
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Size of the on-disk representation
    pub fn num_bytes(&self) -> u64 {
        self.this.bytes() + self.chunk_summary.bytes()
    }

    /// Iterates the on-disk state; panics if there are pending changes.
    pub fn iter(&self) -> impl Stream<Item = T> {
        self.this.iter_entries(self.block_access.clone())
    }

    /// Returns the exact location/size of this chunk (not the whole contiguous extent)
    fn chunk_extent(&self, chunk_id: ChunkId) -> Extent {
        let chunk_id = usize::from64(chunk_id.0);
        let chunk_summary = self.chunks[chunk_id];
        let chunk_size = if chunk_id == self.chunks.len() - 1 {
            self.this.next_chunk_offset - chunk_summary.offset
        } else {
            self.chunks[chunk_id + 1].offset - chunk_summary.offset
        };

        let (extent_offset, extent) = self
            .this
            .extents
            .range((Unbounded, Included(chunk_summary.offset)))
            .next_back()
            .unwrap();
        extent.range(chunk_summary.offset - *extent_offset, chunk_size)
    }

    async fn lookup_by_key_impl<B, F>(&self, key: &B, mut f: F) -> Option<T>
    where
        B: Ord + Debug,
        F: FnMut(&T) -> B,
    {
        assert_eq!(ChunkId(self.chunks.len() as u64), self.this.next_chunk);

        // Find the chunk_id that this key belongs in.
        let chunk_id = match self
            .chunks
            .binary_search_by_key(key, |chunk_summary| f(&chunk_summary.first_entry))
        {
            Ok(index) => ChunkId(index as u64),
            Err(index) if index == 0 => return None, // key is before the first chunk, therefore not present
            Err(index) => ChunkId(index as u64 - 1),
        };

        if let Some(chunk) = self.chunk_cache.lock().unwrap().get(&chunk_id) {
            super_trace!("found {:?} in cache", chunk_id);
            // found in cache
            // Search within this chunk.
            return chunk
                .entries
                .binary_search_by_key(key, f)
                .ok()
                .map(|index| chunk.entries[index]);
        }

        // Lock the chunk so that only one thread reads it
        let _guard = self.chunk_reads.lock(chunk_id).await;

        // Check again in case another thread already read it
        if let Some(chunk) = self.chunk_cache.lock().unwrap().get(&chunk_id) {
            super_trace!("found {:?} in cache after waiting for lock", chunk_id);
            // found in cache
            // Search within this chunk.
            return chunk
                .entries
                .binary_search_by_key(key, f)
                .ok()
                .map(|index| chunk.entries[index]);
        }

        // Read the chunk from disk.
        let chunk_extent = self.chunk_extent(chunk_id);
        super_trace!(
            "reading {:?} at {:?} to lookup {:?}",
            chunk_id,
            chunk_extent,
            key
        );
        let chunk_bytes = self
            .block_access
            .read_raw(chunk_extent, DiskIoType::ReadIndexForLookup)
            .await;
        let (chunk, _consumed): (BlockBasedLogChunk<T>, usize) =
            self.block_access.chunk_from_raw(&chunk_bytes).unwrap();
        assert_eq!(chunk.id, chunk_id);

        // Search within this chunk.
        let result = chunk
            .entries
            .binary_search_by_key(key, f)
            .ok()
            .map(|index| chunk.entries[index]);

        // add to cache
        super_trace!("inserting {:?} to cache", chunk_id);
        self.chunk_cache.lock().unwrap().put(chunk_id, chunk);

        result
    }

    /// Entries must have been added in sorted order, according to the provided
    /// key-extraction function.  Similar to Vec::binary_search_by_key().  The
    /// Guard returned helps the caller ensure that the Entry doesn't live
    /// longer than the reference on the Log (however, since the Entry is Copy,
    /// the caller still needs to be careful to not copy it, then drop the Log,
    /// allowing the Log to be modified before using the copy of the Entry).
    pub async fn lookup_by_key<B, F>(&self, key: &B, f: F) -> Option<BlockBasedLogValueGuard<'_, T>>
    where
        B: Ord + Debug,
        F: FnMut(&T) -> B,
    {
        let value = self.lookup_by_key_impl(key, f).await;
        value.map(|v| BlockBasedLogValueGuard {
            inner: v,
            _marker: &PhantomData,
        })
    }

    /// Update this readonly view to reflect newly-appended chunks.
    pub fn update(
        &mut self,
        phys: SummarizedBlockBasedLogPhys<T>,
        delta: &SummarizedBlockBasedLogFlushDelta<T>,
    ) {
        assert_eq!(delta.first_new_chunk, self.this.next_chunk);
        with_alloctag("ReadOnlySummarizedBlockBasedLog.chunks", || {
            self.chunks.extend_from_slice(&delta.new_chunks)
        });

        self.this = phys.this;
        self.chunk_summary = phys.chunk_summary;
    }
}

#[derive(Debug)]
pub struct SummarizedBlockBasedLogFlushDelta<T: BlockBasedLogEntry> {
    first_new_chunk: ChunkId,
    new_chunks: Vec<BlockBasedLogChunkSummaryEntry<T>>,
}

impl<T: BlockBasedLogEntry> SummarizedBlockBasedLog<T> {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        extent_allocator: Arc<ExtentAllocator>,
        phys: SummarizedBlockBasedLogPhys<T>,
    ) -> Self {
        Self {
            this: BlockBasedLog::open(
                block_access.clone(),
                extent_allocator.clone(),
                phys.this.clone(),
            ),
            chunk_summary: BlockBasedLog::open(
                block_access.clone(),
                extent_allocator.clone(),
                phys.chunk_summary.clone(),
            ),
            readonly: ReadOnlySummarizedBlockBasedLog::open(block_access.clone(), phys).await,
        }
    }

    pub async fn flush(
        &mut self,
    ) -> (
        SummarizedBlockBasedLogPhys<T>,
        SummarizedBlockBasedLogFlushDelta<T>,
    ) {
        let first_new_chunk = self.this.phys.next_chunk;
        let mut new_chunks = Vec::new();
        self.this
            .flush_impl(|_, offset, first_entry| {
                let entry = BlockBasedLogChunkSummaryEntry {
                    offset,
                    first_entry,
                };
                new_chunks.push(entry);
                self.chunk_summary.append(entry);
            })
            .await;
        let (this, chunk_summary) = join(self.this.flush(), self.chunk_summary.flush()).await;

        let phys = SummarizedBlockBasedLogPhys {
            this,
            chunk_summary,
        };
        let delta = SummarizedBlockBasedLogFlushDelta {
            new_chunks,
            first_new_chunk,
        };
        self.readonly.update(phys.clone(), &delta);
        (phys, delta)
    }

    /// Works only if there are no pending entries.
    /// Use flush() to retrieve the phys when there are pending entries.
    pub fn get_phys(&self) -> SummarizedBlockBasedLogPhys<T> {
        assert!(self.this.pending_entries.is_empty());
        assert!(self.chunk_summary.pending_entries.is_empty());
        self.readonly.get_phys()
    }

    pub fn append(&mut self, entry: T) {
        self.this.append(entry);
    }

    pub fn clear(&mut self) {
        self.this.clear();
        self.chunk_summary.clear();
    }

    // Below are helpers that just call through to the readonly struct

    pub fn len(&self) -> u64 {
        self.readonly.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.readonly.is_empty()
    }

    /// Size of the on-disk representation
    pub fn num_bytes(&self) -> u64 {
        self.readonly.num_bytes()
    }

    /// Iterates the on-disk state; panics if there are pending changes.
    pub fn iter(&self) -> impl Stream<Item = T> {
        self.readonly.iter()
    }

    /// See ReadOnlySummarizedBlockBasedLog::lookup_by_key()
    pub async fn lookup_by_key<B, F>(&self, key: &B, f: F) -> Option<BlockBasedLogValueGuard<'_, T>>
    where
        B: Ord + Debug,
        F: FnMut(&T) -> B,
    {
        self.readonly.lookup_by_key(key, f).await
    }
}

pub struct BlockBasedLogValueGuard<'a, T: BlockBasedLogEntry> {
    inner: T,
    _marker: &'a PhantomData<T>,
}

impl<'a, T: BlockBasedLogEntry> std::ops::Deref for BlockBasedLogValueGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct LogOffset(u64);

impl Add<usize> for LogOffset {
    type Output = LogOffset;

    fn add(self, rhs: usize) -> Self::Output {
        LogOffset(self.0 + rhs as u64)
    }
}
impl Sub<LogOffset> for LogOffset {
    type Output = u64;

    fn sub(self, rhs: LogOffset) -> Self::Output {
        self.0 - rhs.0
    }
}

#[derive(
    Serialize, Deserialize, Default, Debug, Copy, Clone, Hash, PartialEq, Eq, Ord, PartialOrd,
)]
pub struct ChunkId(u64);
impl ChunkId {
    pub fn next(&self) -> ChunkId {
        ChunkId(self.0 + 1)
    }
}
