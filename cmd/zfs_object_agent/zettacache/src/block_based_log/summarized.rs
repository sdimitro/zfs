use std::fmt::Debug;
use std::marker::PhantomData;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Context;
use derivative::Derivative;
use futures::future::join;
use futures::StreamExt;
use futures_core::Stream;
use log::*;
use lru::LruCache;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use util::nice_p2size;
use util::super_trace;
use util::tunable;
use util::with_alloctag;
use util::zettacache_stats::DiskIoType;
use util::From64;
use util::LockSet;

use super::BlockBasedLog;
use super::BlockBasedLogChunk;
use super::BlockBasedLogEntry;
use super::BlockBasedLogPhys;
use super::ChunkId;
use super::LogOffset;
use crate::base_types::*;
use crate::block_access::BlockAccess;
use crate::slab_allocator::SlabAccess;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;

tunable! {
    // We primarily use the chunk cache to ensure that when looking up all the
    // entries in an object, we need at most one read from the index.  So we
    // only need as many chunks in the cache as the number of objects that we
    // might be processing concurrently.
    static ref CHUNK_CACHE_ENTRIES: usize = 128;
}

pub trait SummarizedBlockBasedLogEntry: BlockBasedLogEntry {
    type Key: Debug + Copy + Clone + Ord;
    fn key(&self) -> Self::Key;
}

#[derive(Serialize, Deserialize, Derivative, Debug, Clone)]
#[derivative(Default(bound = "T:"))]
#[serde(bound = "T: DeserializeOwned")]
pub struct SummarizedBlockBasedLogPhys<T: SummarizedBlockBasedLogEntry> {
    this: BlockBasedLogPhys<T>,
    chunk_summary: BlockBasedLogPhys<BlockBasedLogChunkSummaryEntry<T>>,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
#[serde(bound = "T: DeserializeOwned")]
pub struct BlockBasedLogChunkSummaryEntry<T: SummarizedBlockBasedLogEntry> {
    offset: LogOffset,
    // Note that we only really need `T::Key` here (like the in-memory SummaryEntry), but we
    // store the entire entry on disk for backwards compatibility.
    first_entry: T,
}
impl<T: SummarizedBlockBasedLogEntry> BlockBasedLogEntry for BlockBasedLogChunkSummaryEntry<T> {}

#[derive(Derivative, Copy)]
#[derivative(Debug, Clone)]
#[repr(packed)]
struct SummaryEntry<T: SummarizedBlockBasedLogEntry> {
    offset: LogOffset,
    first_key: T::Key,
}

pub struct ReadOnlySummarizedBlockBasedLog<T: SummarizedBlockBasedLogEntry> {
    phys: SummarizedBlockBasedLogPhys<T>,
    block_access: Arc<BlockAccess>,
    slab_allocator: Arc<SlabAllocator>,
    chunks: Vec<SummaryEntry<T>>,
    chunk_cache: Mutex<LruCache<ChunkId, BlockBasedLogChunk<T>>>,
    chunk_reads: LockSet<ChunkId>,
}

pub struct SummarizedBlockBasedLog<T: SummarizedBlockBasedLogEntry> {
    readonly: ReadOnlySummarizedBlockBasedLog<T>,
    this: BlockBasedLog<T>,
    chunk_summary: BlockBasedLog<BlockBasedLogChunkSummaryEntry<T>>,
}

pub struct BlockBasedLogValueGuard<'a, T: SummarizedBlockBasedLogEntry> {
    inner: T,
    _marker: &'a PhantomData<T>,
}

impl<'a, T: SummarizedBlockBasedLogEntry> std::ops::Deref for BlockBasedLogValueGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug)]
pub struct SummarizedBlockBasedLogFlushDelta<T: SummarizedBlockBasedLogEntry> {
    first_new_chunk: ChunkId,
    new_chunks: Vec<SummaryEntry<T>>,
}

impl<T: SummarizedBlockBasedLogEntry> SummarizedBlockBasedLogPhys<T> {
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

impl<T: SummarizedBlockBasedLogEntry> ReadOnlySummarizedBlockBasedLog<T> {
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
            .map(|e| SummaryEntry {
                offset: e.offset,
                first_key: e.first_entry.key(),
            })
            .collect::<Vec<_>>()
            .await;
        info!(
            "loaded summary of {} chunks ({} on disk, {} in RAM) in {}ms",
            chunks.len(),
            nice_p2size(phys.chunk_summary.bytes()),
            nice_p2size((chunks.len() * size_of::<SummaryEntry<T>>()) as u64),
            begin.elapsed().as_millis()
        );

        Self {
            phys,
            block_access,
            slab_allocator,
            chunks,
            chunk_cache: Mutex::new(LruCache::new(*CHUNK_CACHE_ENTRIES)),
            chunk_reads: Default::default(),
        }
    }

    pub fn get_phys(&self) -> SummarizedBlockBasedLogPhys<T> {
        self.phys.clone()
    }

    pub fn len(&self) -> u64 {
        self.phys.this.num_entries
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Size of the on-disk representation
    pub fn num_bytes(&self) -> u64 {
        self.phys.this.bytes() + self.phys.chunk_summary.bytes()
    }

    /// Iterates the on-disk state; panics if there are pending changes.
    pub fn iter(&self) -> impl Stream<Item = T> {
        self.phys
            .this
            .iter(self.block_access.clone(), self.slab_allocator.access())
    }

    pub fn iter_chunks(&self) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        self.phys
            .this
            .iter_chunks(self.block_access.clone(), self.slab_allocator.access())
    }

    /// Returns the exact location/size of this chunk (not the whole contiguous extent)
    fn chunk_extent(&self, chunk_id: ChunkId) -> Extent {
        let chunk_id = usize::from64(chunk_id.0);
        let chunk_summary = self.chunks[chunk_id];
        let chunk_size = if chunk_id == self.chunks.len() - 1 {
            self.phys.this.next_chunk_offset - chunk_summary.offset
        } else {
            self.chunks[chunk_id + 1].offset - chunk_summary.offset
        };

        Extent {
            location: self
                .phys
                .this
                .offset_to_location(self.slab_allocator.access(), chunk_summary.offset),
            size: chunk_size,
        }
    }

    /// Return the ChunkId where this key will be found, if present.
    fn lookup_chunk_by_key(&self, key: &T::Key) -> Option<ChunkId> {
        assert_eq!(ChunkId(self.chunks.len() as u64), self.phys.this.next_chunk);

        // Find the chunk_id that this key belongs in.
        match self.chunks.binary_search_by_key(key, |s| s.first_key) {
            Ok(index) => Some(ChunkId(index as u64)),
            // key is before the first chunk, therefore not present
            Err(index) if index == 0 => None,
            Err(index) => Some(ChunkId(index as u64 - 1)),
        }
    }

    /// Returns (value, chunk_cache_hit), where the value is the value corresponding
    /// to the key argument if found, and chunk_cache_hit that tells us whether we found
    /// the value on the chunk cache (true) or had to reach out to disk (false).
    async fn lookup_by_key_impl(&self, key: &T::Key) -> (Option<T>, bool) {
        let chunk_id = match self.lookup_chunk_by_key(key) {
            Some(chunk_id) => chunk_id,
            None => return (None, false),
        };

        if let Some(chunk) = self.chunk_cache.lock().unwrap().get(&chunk_id) {
            super_trace!("found {:?} in cache", chunk_id);
            // found in cache
            // Search within this chunk.
            return (
                chunk
                    .entries
                    .binary_search_by_key(key, |e| e.key())
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
                    .binary_search_by_key(key, |e| e.key())
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
        let (chunk, _consumed): (BlockBasedLogChunk<T>, usize) = self
            .block_access
            .chunk_from_raw(&chunk_bytes)
            .with_context(|| format!("reading {chunk_id:?} at {chunk_extent:?} to lookup {key:?}"))
            .unwrap();

        assert_eq!(chunk.id, chunk_id);

        // Search within this chunk.
        let result = chunk
            .entries
            .binary_search_by_key(key, |e| e.key())
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
    pub async fn lookup_by_key(
        &self,
        key: &T::Key,
    ) -> (Option<BlockBasedLogValueGuard<'_, T>>, bool) {
        let (value, chunk_cache_hit) = self.lookup_by_key_impl(key).await;
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
        assert_eq!(delta.first_new_chunk, self.phys.this.next_chunk);
        with_alloctag("ReadOnlySummarizedBlockBasedLog.chunks", || {
            self.chunks.extend_from_slice(&delta.new_chunks)
        });

        self.phys = phys;
    }
}

impl<T: SummarizedBlockBasedLogEntry> SummarizedBlockBasedLog<T> {
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
                new_chunks.push(SummaryEntry {
                    offset,
                    first_key: first_entry.key(),
                });
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
    pub async fn lookup_by_key(
        &self,
        key: &T::Key,
    ) -> (Option<BlockBasedLogValueGuard<'_, T>>, bool) {
        self.readonly.lookup_by_key(key).await
    }
}
