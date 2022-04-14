use std::cmp::max;
use std::cmp::min;
use std::fmt::Debug;
use std::iter;
use std::marker::PhantomData;
use std::ops::Add;
use std::ops::Sub;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Context;
use bytesize::ByteSize;
use derivative::Derivative;
use futures::future::join;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use futures_core::Stream;
use log::*;
use lru::LruCache;
use more_asserts::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use tokio_stream::wrappers::ReceiverStream;
use util::measure;
use util::nice_p2size;
use util::super_trace;
use util::tunable;
use util::with_alloctag;
use util::zettacache_stats::DiskIoType;
use util::From64;
use util::LockSet;

use crate::base_types::*;
use crate::block_access::BlockAccess;
use crate::block_access::EncodeType;
use crate::slab_allocator::SlabAccess;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::slab_allocator::SlabId;

tunable! {
    // ENTRIES_PER_CHUNK is chosen so that chunks of the Index will be 8KB on disk, with a
    // minimum of padding.  The size in bytes of each field is:
    // chunk_to_raw(BlockBasedLogChunkBorrowed<IndexEntry>):
    //   51: BlockHeader (JSON)
    //    1: NULL byte to terminate JSON string
    // BlockBasedLogChunkBorrowed:
    //  <=9: id: ChunkId(u64), varint
    //  <=9: offset: LogOffset(u64), varint
    //    3: entries slice length
    // 8048: = 337 * (1+23): slice of slices (1=slice len, 23 = size_of<IndexEntryPhys>)
    // >=23: padding.  Typically, 35 bytes of padding is observed.
    static ref ENTRIES_PER_CHUNK: usize = 337;
    // Note: kernel sends writes to disk in at most 256K chunks (at least with nvme driver)
    static ref WRITE_AGGREGATION_SIZE: ByteSize = ByteSize::kib(256);
    // We primarily use the chunk cache to ensure that when looking up all the
    // entries in an object, we need at most one read from the index.  So we
    // only need as many chunks in the cache as the number of objects that we
    // might be processing concurrently.
    static ref CHUNK_CACHE_ENTRIES: usize = 128;
    // This can be increased if we need to have multiple (16MB) extents being
    // read at once.  Each one would typically be read from a different disk, so
    // this may be needed if we the throughput of multiple disks.
    static ref ITER_CONCURRENT_READS: usize = 1;
    // Number of chunks to buffer in the channel; experimentally determinded
    // that >100 gives good performance.
    static ref ITER_CHUNKS_TO_BUFFER: usize = 1000;
}

#[derive(Derivative, Serialize, Deserialize, Debug, Clone)]
#[derivative(Default(bound = "T:"))]
pub struct BlockBasedLogPhys<T: BlockBasedLogEntry> {
    slabs: Vec<SlabId>,
    next_chunk: ChunkId,
    next_chunk_offset: LogOffset, // logical byte offset of next chunk to write
    num_entries: u64,
    entry_type: PhantomData<T>,
}

impl<T: BlockBasedLogEntry> BlockBasedLogPhys<T> {
    pub fn clear(&mut self, slab_allocator: &SlabAllocator) {
        for &slab in &self.slabs {
            slab_allocator.free(slab);
        }
        *self = Default::default();
    }

    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        for &slab in &self.slabs {
            builder.claim(slab);
        }
    }

    // Since &self is not captured by the returned Stream (its extent list is cloned), callers
    // must ensure that the disk space represented by the extents is not overwritten before the
    // stream terminates.  i.e. do not call .clear().
    pub fn iter_chunks(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        let slab_size = slab_access.slab_size();
        let extents = self
            .slabs
            .iter()
            .enumerate()
            .map(|(slab_index, &slab_id)| {
                // truncate last extent to log size
                let offset = LogOffset(slab_index as u64 * slab_size);
                let extent = slab_access.slab_id_to_extent(slab_id);
                extent.range(0, min(extent.size, self.next_chunk_offset - offset))
            })
            .collect::<Vec<_>>();

        // Just buffer a single (16MB) extent between the two tasks.
        let (extent_tx, mut extent_rx) = tokio::sync::mpsc::channel(1);

        {
            let block_access = block_access.clone();
            measure!("BlockBasedLogPhys::iter_chunks() reader").spawn(async move {
                let block_access = &*block_access;
                stream::iter(
                    extents
                        .into_iter()
                        .map(|extent| block_access.read_raw(extent, DiskIoType::MaintenanceRead)),
                )
                .buffered(*ITER_CONCURRENT_READS)
                .for_each(|extent_bytes| async {
                    extent_tx.send(extent_bytes).await.ok();
                })
                .await;
            });
        }

        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(*ITER_CHUNKS_TO_BUFFER);

        let next_chunk = self.next_chunk;
        measure!("BlockBasedLogPhys::iter_chunks() deserializer").spawn(async move {
            let mut chunk_id = ChunkId(0);
            while let Some(extent_bytes) = extent_rx.recv().await {
                let mut total_consumed = 0;
                while total_consumed < extent_bytes.len() {
                    // XXX handle checksum error here
                    let (chunk, consumed): (BlockBasedLogChunk<T>, usize) = block_access
                        .chunk_from_raw(&extent_bytes[total_consumed..])
                        .with_context(|| format!("{:?}", chunk_id))
                        .unwrap();
                    assert_eq!(chunk.id, chunk_id);
                    if chunk_tx.send(chunk).await.is_err() {
                        break;
                    }
                    chunk_id = chunk_id.next();
                    total_consumed += consumed;
                    if chunk_id == next_chunk {
                        break;
                    }
                }
            }
        });

        ReceiverStream::new(chunk_rx)
    }

    pub fn iter(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = T> {
        self.iter_chunks(block_access, slab_access)
            .flat_map(|chunk| stream::iter(chunk.entries.into_iter()))
    }

    pub fn bytes(&self) -> u64 {
        self.next_chunk_offset.0
    }

    pub fn len(&self) -> u64 {
        self.num_entries
    }

    pub fn capacity_bytes(&self, slab_access: &SlabAccess) -> u64 {
        self.slabs.len() as u64 * slab_access.slab_size()
    }

    fn next_write_location(&self, slab_allocator: &SlabAllocator) -> Option<Extent> {
        self.slabs
            .last()
            .map(|&slab_id| {
                let slab_offset =
                    LogOffset((self.slabs.len() - 1) as u64 * slab_allocator.slab_size());
                let offset_within_extent = self.next_chunk_offset - slab_offset;
                let extent = slab_allocator.slab_id_to_extent(slab_id);
                extent.range(offset_within_extent, extent.size - offset_within_extent)
            })
            .filter(|extent| extent.size > 0)
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
    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.this.claim(builder);
        self.chunk_summary.claim(builder);
    }

    pub fn iter(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = T> {
        self.this.iter(block_access, slab_access)
    }

    pub fn iter_chunks(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        self.this.iter_chunks(block_access, slab_access)
    }

    pub fn iter_summary_chunks(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = BlockBasedLogChunk<BlockBasedLogChunkSummaryEntry<T>>> {
        self.chunk_summary.iter_chunks(block_access, slab_access)
    }

    pub fn bytes(&self) -> u64 {
        self.chunk_summary.bytes() + self.this.bytes()
    }

    pub fn capacity_bytes(&self, slab_access: &SlabAccess) -> u64 {
        self.chunk_summary.capacity_bytes(slab_access) + self.this.capacity_bytes(slab_access)
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
    slab_allocator: Arc<SlabAllocator>,
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
    slab_allocator: Arc<SlabAllocator>,
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

impl<T: BlockBasedLogEntry> BlockBasedLogChunk<T> {
    pub fn entries(&self) -> &[T] {
        &self.entries
    }
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
        slab_allocator: Arc<SlabAllocator>,
        phys: BlockBasedLogPhys<T>,
    ) -> BlockBasedLog<T> {
        BlockBasedLog {
            block_access,
            slab_allocator,
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

    pub fn push(&mut self, entry: T) {
        with_alloctag("BlockBasedLog.pending_entries", || {
            self.pending_entries.push(entry)
        });
        // XXX if too many pending, initiate flush?
    }

    pub fn append(&mut self, mut list: Vec<T>) {
        if self.pending_entries.is_empty() {
            self.pending_entries = list;
        } else {
            self.pending_entries.append(&mut list);
        }
    }

    async fn flush_impl<F>(&mut self, mut new_chunk_fn: F)
    where
        F: FnMut(ChunkId, LogOffset, T),
    {
        let writes_stream = FuturesUnordered::new();

        let mut remaining_entries = self.pending_entries.as_slice();
        let mut max_entries = None;
        while !remaining_entries.is_empty() {
            // Calculate the minimum of these 2 or 3 values (depending on if max_entries is Some)
            let num_entries = iter::once(remaining_entries.len())
                .chain(iter::once(*ENTRIES_PER_CHUNK))
                .chain(max_entries)
                .min()
                .unwrap();
            assert_gt!(num_entries, 0);
            max_entries = None;

            let (head, tail) = remaining_entries.split_at(num_entries);
            let chunk = BlockBasedLogChunkBorrowed {
                id: self.phys.next_chunk,
                offset: self.phys.next_chunk_offset,
                entries: head,
            };

            // XXX I think we only want to use Bincode for the main index?
            let raw_chunk = self.block_access.chunk_to_raw(EncodeType::Bincode, &chunk);
            let extent = match self.phys.next_write_location(&self.slab_allocator) {
                Some(extent) if extent.size >= raw_chunk.len() as u64 => extent,
                Some(_) => {
                    // Not enough space at end of the current slab, try a smaller chunk.  We need
                    // to fill the entire slab, otherwise iter_impl() won't know where to stop.
                    // Note that we need to fit at least one entry in the smallest-size chunk (1
                    // sector), so that we have a "first" entry to pass to new_chunk_fn().
                    assert_gt!(num_entries, 1);
                    max_entries = Some(max(1, num_entries / 2));
                    continue;
                }
                None => {
                    // Last slab has been fully written, allocate a new one
                    let slab = self.slab_allocator.allocate_reserved();
                    self.phys.slabs.push(slab);
                    self.slab_allocator.slab_id_to_extent(slab)
                }
            };
            let raw_size = raw_chunk.len() as u64;
            assert_ge!(extent.size, raw_size);

            new_chunk_fn(
                self.phys.next_chunk,
                self.phys.next_chunk_offset,
                *head.first().unwrap(),
            );

            self.phys.num_entries += head.len() as u64;
            self.phys.next_chunk = self.phys.next_chunk.next();
            self.phys.next_chunk_offset.0 += raw_size;

            writes_stream.push(self.block_access.write_raw(
                extent.location,
                raw_chunk,
                DiskIoType::MaintenanceWrite,
            ));

            // head is consumed
            remaining_entries = tail;
        }

        writes_stream.count().await;
        self.pending_entries.truncate(0);
    }

    pub fn clear(&mut self) {
        self.pending_entries.clear();
        self.phys.clear(&self.slab_allocator);
    }

    /// Iterates the on-disk state; panics if there are pending changes.
    pub fn iter(&self) -> impl Stream<Item = T> {
        assert!(self.pending_entries.is_empty());
        self.phys
            .iter(self.block_access.clone(), self.slab_allocator.access())
    }
}
impl<T: BlockBasedLogEntry> ReadOnlySummarizedBlockBasedLog<T> {
    pub async fn open(
        block_access: Arc<BlockAccess>,
        slab_allocator: Arc<SlabAllocator>,
        phys: SummarizedBlockBasedLogPhys<T>,
    ) -> Self {
        // load in summary from disk
        let begin = Instant::now();
        // XXX how to measure memory usage, since it's gathered async?  Copy it later?  Or just rely
        // on the log statement below?
        let chunks = phys
            .chunk_summary
            .iter(block_access.clone(), slab_allocator.access())
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
            slab_allocator,
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
        self.this
            .iter(self.block_access.clone(), self.slab_allocator.access())
    }

    pub fn iter_chunks(&self) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        self.this
            .iter_chunks(self.block_access.clone(), self.slab_allocator.access())
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

        let slab_size = self.slab_allocator.slab_size();
        let extent = self
            .slab_allocator
            .slab_id_to_extent(self.this.slabs[usize::from64(chunk_summary.offset.0 / slab_size)]);
        extent.range(chunk_summary.offset.0 % slab_size, chunk_size)
    }

    /// Returns (value, chunk_cache_hit), where the value is the value corresponding
    /// to the key argument if found, and chunk_cache_hit that tells us whether we found
    /// the value on the chunk cache (true) or had to reach out to disk (false).
    async fn lookup_by_key_impl<B, F>(&self, key: &B, mut f: F) -> (Option<T>, bool)
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
            // key is before the first chunk, therefore not present
            Err(index) if index == 0 => return (None, false),
            Err(index) => ChunkId(index as u64 - 1),
        };

        if let Some(chunk) = self.chunk_cache.lock().unwrap().get(&chunk_id) {
            super_trace!("found {:?} in cache", chunk_id);
            // found in cache
            // Search within this chunk.
            return (
                chunk
                    .entries
                    .binary_search_by_key(key, f)
                    .ok()
                    .map(|index| chunk.entries[index]),
                true,
            );
        }

        // Lock the chunk so that only one thread reads it
        let _guard = self.chunk_reads.lock(chunk_id).await;

        // Check again in case another thread already read it
        if let Some(chunk) = self.chunk_cache.lock().unwrap().get(&chunk_id) {
            super_trace!("found {:?} in cache after waiting for lock", chunk_id);
            // found in cache
            // Search within this chunk.
            return (
                chunk
                    .entries
                    .binary_search_by_key(key, f)
                    .ok()
                    .map(|index| chunk.entries[index]),
                true,
            );
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

        (result, false)
    }

    /// Entries must have been added in sorted order, according to the provided
    /// key-extraction function.  Similar to Vec::binary_search_by_key().  The
    /// Guard returned helps the caller ensure that the Entry doesn't live
    /// longer than the reference on the Log (however, since the Entry is Copy,
    /// the caller still needs to be careful to not copy it, then drop the Log,
    /// allowing the Log to be modified before using the copy of the Entry).
    ///
    /// Returns (value, chunk_cache_hit), where the value is the value corresponding
    /// to the key argument if found, and chunk_cache_hit that tells us whether we found
    /// the value on the chunk cache (true) or had to reach out to disk (false).
    pub async fn lookup_by_key<B, F>(
        &self,
        key: &B,
        f: F,
    ) -> (Option<BlockBasedLogValueGuard<'_, T>>, bool)
    where
        B: Ord + Debug,
        F: FnMut(&T) -> B,
    {
        let (value, chunk_cache_hit) = self.lookup_by_key_impl(key, f).await;
        (
            value.map(|v| BlockBasedLogValueGuard {
                inner: v,
                _marker: &PhantomData,
            }),
            chunk_cache_hit,
        )
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
        slab_allocator: Arc<SlabAllocator>,
        phys: SummarizedBlockBasedLogPhys<T>,
    ) -> Self {
        Self {
            this: BlockBasedLog::open(
                block_access.clone(),
                slab_allocator.clone(),
                phys.this.clone(),
            ),
            chunk_summary: BlockBasedLog::open(
                block_access.clone(),
                slab_allocator.clone(),
                phys.chunk_summary.clone(),
            ),
            readonly: ReadOnlySummarizedBlockBasedLog::open(
                block_access.clone(),
                slab_allocator,
                phys,
            )
            .await,
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
                self.chunk_summary.push(entry);
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

    pub fn append(&mut self, list: Vec<T>) {
        self.this.append(list);
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

    pub fn iter_chunks(&self) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        self.readonly.iter_chunks()
    }

    /// See ReadOnlySummarizedBlockBasedLog::lookup_by_key()
    pub async fn lookup_by_key<B, F>(
        &self,
        key: &B,
        f: F,
    ) -> (Option<BlockBasedLogValueGuard<'_, T>>, bool)
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
