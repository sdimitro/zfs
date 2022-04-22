pub mod summarized;

use std::cmp::max;
use std::fmt::Debug;
use std::iter;
use std::marker::PhantomData;
use std::ops::Add;
use std::ops::Sub;
use std::sync::Arc;

use bytesize::ByteSize;
use derivative::Derivative;
use futures::stream;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use futures_core::Stream;
use more_asserts::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use tokio_stream::wrappers::ReceiverStream;
use util::measure;
use util::tunable;
use util::with_alloctag;
use util::zettacache_stats::DiskIoType;
use util::From64;

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
    // This can be increased if we need to have multiple (16MB) extents being
    // read at once.  Each one would typically be read from a different disk, so
    // this may be needed if we the throughput of multiple disks.
    static ref ITER_CONCURRENT_READS: usize = 1;
    // Number of chunks to buffer in the channel; experimentally determinded
    // that >100 gives good performance.
    static ref ITER_CHUNKS_TO_BUFFER: usize = 1000;
}

pub trait BlockBasedLogEntry:
    'static + Serialize + DeserializeOwned + Debug + Copy + Clone + Unpin + Send + Sync
{
}

#[derive(Derivative, Serialize, Deserialize, Debug, Clone)]
#[derivative(Default(bound = "T:"))]
#[serde(bound = "T: DeserializeOwned")]
pub struct BlockBasedLogPhys<T: BlockBasedLogEntry> {
    slabs: Vec<SlabId>,
    next_chunk: ChunkId,
    next_chunk_offset: LogOffset, // logical byte offset of next chunk to write
    num_entries: u64,
    entry_type: PhantomData<T>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(bound = "T: DeserializeOwned")]
pub struct BlockBasedLogChunk<T: BlockBasedLogEntry> {
    id: ChunkId,
    offset: LogOffset,
    entries: Vec<T>,
}

#[derive(Serialize, Debug)]
pub struct BlockBasedLogChunkBorrowed<'a, T: BlockBasedLogEntry> {
    id: ChunkId,
    offset: LogOffset,
    entries: &'a [T],
}

pub struct BlockBasedLog<T: BlockBasedLogEntry> {
    block_access: Arc<BlockAccess>,
    slab_allocator: Arc<SlabAllocator>,
    phys: BlockBasedLogPhys<T>,
    pending_entries: Vec<T>,
}

impl<T: BlockBasedLogEntry> BlockBasedLogChunk<T> {
    pub fn entries(&self) -> &[T] {
        &self.entries
    }
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

    fn written_extents<'a>(
        &'a self,
        slab_access: &'a SlabAccess,
    ) -> impl DoubleEndedIterator<Item = (LogOffset, Extent)> + 'a {
        self.allocated_extents(slab_access)
            .map(|(offset, extent)| (offset, extent.trim_end(self.next_chunk_offset - offset)))
    }

    fn allocated_extents<'a>(
        &'a self,
        slab_access: &'a SlabAccess,
    ) -> impl DoubleEndedIterator<Item = (LogOffset, Extent)> + 'a {
        self.slabs.iter().enumerate().map(|(slab_index, &slab_id)| {
            let offset = LogOffset((slab_index as u64) * slab_access.slab_size());
            let extent = slab_access.slab_id_to_extent(slab_id);
            (offset, extent)
        })
    }

    fn next_extent_to_write(&self, slab_access: &SlabAccess) -> Option<Extent> {
        self.allocated_extents(slab_access)
            .last()
            .map(|(offset, extent)| extent.trim_start(self.next_chunk_offset - offset))
            .filter(|extent| extent.size > 0)
    }

    fn offset_to_location(&self, slab_access: &SlabAccess, offset: LogOffset) -> DiskLocation {
        let slab_size = slab_access.slab_size();
        let slab_index = usize::from64(offset.0 / slab_size);
        let relative_offset = offset.0 % slab_size;
        slab_access
            .slab_id_to_extent(self.slabs[slab_index])
            .location
            + relative_offset
    }

    // Since &self is not captured by the returned Stream (its extent list is cloned), callers
    // must ensure that the disk space represented by the extents is not overwritten before the
    // stream terminates.  i.e. do not call .clear().
    pub fn iter_chunks(
        &self,
        block_access: Arc<BlockAccess>,
        slab_access: &SlabAccess,
    ) -> impl Stream<Item = BlockBasedLogChunk<T>> {
        let extents = self
            .written_extents(slab_access)
            .map(|(_, extent)| extent)
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
            while let Some(extent_bytes) = extent_rx.recv().await {
                let mut total_consumed = 0;
                while total_consumed < extent_bytes.len() {
                    // XXX handle checksum error here
                    let (chunk, consumed): (BlockBasedLogChunk<T>, usize) = block_access
                        .chunk_from_raw(&extent_bytes[total_consumed..])
                        .unwrap();
                    let chunk_id = chunk.id;
                    if chunk_tx.send(chunk).await.is_err() {
                        break;
                    }
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
            let extent = match self.phys.next_extent_to_write(self.slab_allocator.access()) {
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

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct LogOffset(u64);

impl Add<usize> for LogOffset {
    type Output = Self;
    fn add(self, rhs: usize) -> Self::Output {
        self + rhs as u64
    }
}
impl Add<u64> for LogOffset {
    type Output = Self;
    fn add(self, rhs: u64) -> Self::Output {
        Self(self.0 + rhs)
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
