pub mod zcdb;

use std::collections::btree_map;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::mem;
use std::mem::size_of;
use std::ops::Bound::Excluded;
use std::ops::Bound::Included;
use std::ops::Bound::Unbounded;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Result;
use bytes::Bytes;
use bytesize::ByteSize;
use either::Either;
use futures::future;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::Future;
use futures::FutureExt;
use log::*;
use lru::LruCache;
use more_asserts::*;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use sysinfo::System;
use sysinfo::SystemExt;
use tokio::sync::mpsc;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::time::sleep_until;
use tokio::time::timeout_at;
use util::concurrent_batch::ConcurrentBatch;
use util::lock_non_send;
use util::measure;
use util::nice_p2size;
use util::super_trace;
use util::tunable;
use util::tunable::LayeredTunable;
use util::tunable::Percent;
use util::with_alloctag;
use util::with_alloctag_hf;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::CacheStats;
use util::zettacache_stats::DiskIoType;
use util::AlignedBytes;
use util::From64;
use util::LockSet;
use util::LockedItem;
use uuid::Uuid;

use crate::atime_histogram::AtimeHistogram;
use crate::atime_histogram::AtimeHistogramPhys;
use crate::base_types::*;
use crate::block_access::*;
use crate::block_allocator::BlockAllocator;
use crate::block_allocator::BlockAllocatorBuilder;
use crate::block_allocator::BlockAllocatorPhys;
use crate::block_based_log::*;
use crate::checkpoint::CheckpointId;
use crate::checkpoint::CheckpointPhys;
use crate::features::check_features;
use crate::features::SUPPORTED_FEATURES;
use crate::index::*;
use crate::pool_id::PoolGuidMapping;
use crate::pool_id::PoolGuidMappingPhys;
use crate::size_histogram::SizeHistogramPhys;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::slab_allocator::SlabAllocatorPhys;
use crate::slab_allocator::RESERVED_SLABS_PCT;
use crate::superblock::DiskPhys;
use crate::superblock::PrimaryPhys;
use crate::superblock::SUPERBLOCK_SIZE;

#[derive(Debug)]
struct GhostCacheSizePct(Percent);
impl LayeredTunable for GhostCacheSizePct {
    type Input = Percent;
    fn convert(input: Self::Input) -> Result<Self> {
        // This value needs to stay < 200 to safely avoid using up all available metadata space
        // in the cache.
        Ok(GhostCacheSizePct(Percent::new(
            input.as_percent().min(200.0),
        )))
    }
}
tunable! { static ref GHOST_CACHE_SIZE_PCT: GhostCacheSizePct = GhostCacheSizePct(Percent::new(100.0)); }

tunable! {
    static ref DEFAULT_CHECKPOINT_SIZE_PCT: Percent = Percent::new(0.1);

    // In order to keep enough free space available in the cache to ingest data during a merge,
    // keep at least 5% of the cache "free".  We need to have slop for the rebalance code to be
    // able to consolidate slabs (to create empty slabs) to accomodate block size changes in the
    // workload.  This target includes both free blocks within the BlockAllocator, and free slabs
    // within the SlabAllocator which are available to the BlockAllocator.
    static ref TARGET_FREE_BLOCKS_PCT: Percent = Percent::new(5.0);

    // Keep the total footprint for the pending changes and index cache data at about 12% of
    // total memory.  The above tuning for eviction provides a 1TB "buffer" for insertions (on a
    // 100TB config) during a merge. Using 5% for pending changes provides sufficient memory to
    // absorb the same 1TB of insertions (on a 128GB config).
    static ref PENDING_CHANGES_MEM_PCT: Percent = Percent::new(5.0);
    static ref INDEX_CACHE_ENTRIES_MEM_PCT: Percent = Percent::new(7.0);

    static ref CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

    static ref MERGE_PROGRESS_CHUNK: usize = 1_000_000;

    static ref QUANTILES_IN_SIZE_HISTOGRAM: usize = 100;

    // Buffers for incomming data blocks: the "demand" buffer is for read-miss blocks. The
    // "speculative" buffer is for blocks being written. Note that ingesting a single block from
    // an object can result in "inflation" since the entire object must be held in memory. But
    // this is mitigated by the fact that we typically ingest the entire object on writes, and
    // make a copy of the block to ingest on read (so we don't hold the object).
    static ref CACHE_INSERT_DEMAND_BUFFER_SIZE: ByteSize = ByteSize::mib(256);
    static ref CACHE_INSERT_SPECULATIVE_BUFFER_SIZE: ByteSize = ByteSize::mib(256);
    static ref CACHE_WAIT_INSERT: bool = false;

    // Limit this to half the read queue depth (per disk) so that we don't crowd
    // out normal reads too much.  Note that since writes aggregate, they
    // typically won't be the io bottleneck.
    static ref CACHE_REBALANCE_CONCURRENCY_LIMIT: usize = *DISK_READ_MAX_QUEUE_DEPTH / 2;

    // If non-zero, the lookup() function will fail randomly every specified number of requests
    static ref LOOKUP_FAIL_RANDOM: u32 = 0;

    static ref ATIME_INTERVAL: Duration = Duration::from_secs(10);
    static ref STATS_INTERVAL: Duration = Duration::from_secs(1);

    // Don't start a merge due to evacuating (getting whole free slabs) unless we intend to free
    // up at least this much space.
    static ref EVACUATION_MIN_BATCH_PCT: Percent = Percent::new(0.2);

    // Don't start a merge due to evicting (removing LRU blocks from cache) unless we intend to
    // free up at least this much space.
    static ref EVICTION_MIN_BATCH_PCT: Percent = Percent::new(0.5);

    // Percent of time to corrupt data on inserts and lookups
    static ref CORRUPT_INSERT_PCT: Percent = Percent::new(0.0);
    static ref CORRUPT_LOOKUP_PCT: Percent = Percent::new(0.0);
    static ref CORRUPTION_FILL: u8 = 0x31; // fill blocks with '1'
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MergeProgressPhys {
    rebalance_log: Option<BlockBasedLogPhys<RebalanceLogEntry>>,
    operation_log: BlockBasedLogPhys<OperationLogEntry>,
    new_index: IndexRunPhys,
}

impl MergeProgressPhys {
    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.operation_log.claim(builder);
        self.new_index.claim(builder);
        if let Some(rebalance_log) = self.rebalance_log.as_ref() {
            rebalance_log.claim(builder);
        }
    }
}

/// A PendingChange is the in-core data structure for tracking changes to the index between merges.
/// Two types of events are tracked: insertions and lookup hits (atime update). Note that there is
/// no removal event here because we do not allow removes in general. Cache content is only removed
/// during the merge/eviction task.
#[derive(Debug, Clone, Copy)]
enum PendingChange {
    Insert(IndexValue),
    UpdateAtime(UpdateAtime),
}

#[derive(Debug, Clone, Copy)]
#[repr(packed)]
struct UpdateAtime(IndexValue, Atime);

#[derive(Clone)]
pub struct ZettaCache(Arc<Inner>);
impl Deref for ZettaCache {
    type Target = Inner;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct Inner {
    block_access: Arc<BlockAccess>,
    slab_allocator: Arc<SlabAllocator>,

    // lock ordering: index first then state
    old_index: Arc<tokio::sync::RwLock<IndexRun>>,
    new_index: Arc<tokio::sync::RwLock<Option<ReadOnlyIndexRun>>>, // merging into this
    // XXX may need to break up this big lock.  At least we aren't holding it while doing i/o
    state: Arc<tokio::sync::Mutex<ZettaCacheState>>,
    outstanding_lookups: LockSet<IndexKey>,
    stats: Arc<CacheStats>,
    timebase: Instant, // used when collecting stats
    demand_buffer_bytes_available: Arc<Semaphore>,
    speculative_buffer_bytes_available: Arc<Semaphore>,
    cache_runtime_id: Uuid,
    pool_guids: PoolGuidMapping,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
pub enum OperationLogEntry {
    Insert(IndexKey, IndexValue),
}
impl OnDisk for OperationLogEntry {}
impl BlockBasedLogEntry for OperationLogEntry {}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
struct RebalanceLogEntry {
    old: Extent,
    new: Option<DiskLocation>,
}
impl OnDisk for RebalanceLogEntry {}
impl BlockBasedLogEntry for RebalanceLogEntry {}

#[derive(Debug)]
struct IndexMessage {
    last_key: IndexKey,
    entries: Vec<IndexEntry>,
    frees: Vec<Extent>,
    cache_updates: Vec<IndexEntry>,
    obsoleted: AtimeHistogramPhys, // entries obsoleted from old index, since last MergeProgress
}

#[derive(Debug)]
struct MergeProgress {
    new_index: IndexRunPhys,
    obsoleted: AtimeHistogramPhys,
    index_delta: IndexFlushDelta,
    frees: Vec<Extent>,
    cache_updates: Vec<IndexEntry>,
}

#[allow(clippy::large_enum_variant)]
enum MergeMessage {
    Progress(MergeProgress),
    Complete(IndexRun),
}

impl MergeMessage {
    /// Compose a progress update to send to the checkpoint task.
    async fn new_progress(
        next_index: &mut IndexRun,
        frees: Vec<Extent>,
        cache_updates: Vec<IndexEntry>,
        obsoleted: AtimeHistogramPhys,
    ) -> Self {
        let timer = Instant::now();
        let free_count = frees.len();
        let cache_updates_count = cache_updates.len();
        let (new_index, index_delta) = next_index.flush().await;
        let message = MergeProgress {
            new_index,
            index_delta,
            obsoleted,
            frees,
            cache_updates,
        };
        debug!("sending progress: index with {} entries ({}) last is {:?} flushed in {}ms, and {} frees, and {} cache_updates.",
            next_index.len(),
            nice_p2size(next_index.num_bytes()),
            next_index.last_key(), timer.elapsed().as_millis(),
            free_count,
            cache_updates_count);
        Self::Progress(message)
    }
}

#[derive(Debug)]
struct RebalanceState {
    map: BTreeMap<Extent, Option<DiskLocation>>,
    log_phys: BlockBasedLogPhys<RebalanceLogEntry>,
}

impl RebalanceState {
    fn remap(&self, extent: Extent) -> Option<DiskLocation> {
        if let Some((old, new)) = self
            .map
            .range((Unbounded, Included(extent.location)))
            .next_back()
        {
            if old.contains(&extent) {
                match new {
                    Some(new_location) => {
                        // This represents the offset of the passed in extent, into the extent
                        // that was moved as part of the rebalance operation. For example,
                        // multiple contiguously allocated blocks maybe have been moved via a
                        // single extent. Thus, to remap one of those blocks' to it's new
                        // location on disk, we need this offset (this offset is maintained when
                        // the blocks are copied).
                        let offset = extent.location - old.location;

                        return Some(DiskLocation::new(
                            new_location.disk(),
                            new_location.offset() + offset,
                        ));
                    }
                    None => {
                        // This means the extent was part of a rebalance operation, but when
                        // attempting to remap the old location to a new location, the allocation
                        // failed. Thus, the old extent does not have new location, and it will
                        // be invalid after the rebalance completes.
                        return None;
                    }
                }
            }
        }

        // If we reach this point, we didn't find an extent in the mapping that contains the passed
        // in extent, which means the passed in extent was not remapped; thus, we simply
        // return the old extent's location.
        Some(extent.location)
    }
}

#[derive(Debug)]
struct MergeState {
    rebalance: Option<RebalanceState>,
    old_pending_changes: PendingChanges,
    old_operation_log_phys: BlockBasedLogPhys<OperationLogEntry>,
    eviction_cutoff: Atime,
    ghost_cutoff: Atime,
    stats: Arc<CacheStats>,
}

impl MergeState {
    /// This task offloads the task of writing out the next index from the merge task.
    /// This allows the merge to proceed in parallel with the writes to disk. Relatively
    /// large chunks of the new index are provided to make the IO as efficient as possible.
    async fn next_index_task(
        &self,
        mut merge_rx: mpsc::Receiver<IndexMessage>,
        checkpoint_tx: mpsc::Sender<MergeMessage>,
        next_index: &mut IndexRun,
    ) {
        let begin = Instant::now();
        while let Some(message) = merge_rx.recv().await {
            next_index.append(message.entries);
            // The "last key" from the appended entries may not be the last key we actually
            // processed in the merge (e.g, we may have evicted some entries later)
            next_index.update_last_key(message.last_key);
            checkpoint_tx
                .send(
                    MergeMessage::new_progress(
                        next_index,
                        message.frees,
                        message.cache_updates,
                        message.obsoleted,
                    )
                    .await,
                )
                .await
                .unwrap_or_else(|e| panic!("couldn't send: {}", e));
        }

        info!(
            "wrote next index with {} entries ({}) in {:.1}s ({:.1}MB/s)",
            next_index.len(),
            nice_p2size(next_index.num_bytes()),
            begin.elapsed().as_secs_f64(),
            (next_index.num_bytes() as f64 / 1024f64 / 1024f64) / begin.elapsed().as_secs_f64(),
        );
    }

    /// This function runs in an async task to merge a set of pending changes with the current
    /// on-disk index in order to produce a new up-to-date on-disk index. It sends periodic
    /// "progress updates" (including block frees) to the checkpoint task.
    async fn merge_task(
        &self,
        tx: mpsc::Sender<IndexMessage>,
        old_index_lock: Arc<tokio::sync::RwLock<IndexRun>>,
        start_key: Option<IndexKey>,
        block_access: &BlockAccess,
    ) {
        // We don't currently support concurrent free()'s while the rebalance is in-progress. Thus,
        // we need to do the rebalance first, prior to moving forward with the merge.
        self.rebalance(block_access).await;

        let begin = Instant::now();

        /// The merge task sends incremental progress messages to the checkpoint task
        /// so that the progress can be persisted (and restarted if the agent dies) and
        /// also so that space from evicted blocks can become available without needing
        /// to wait for merge completion. Buffers for accumulated work are pre-allocated
        /// to avoid the cost of growing those buffers during the merge.
        struct Progress {
            tx: mpsc::Sender<IndexMessage>,
            last_key: Option<IndexKey>,
            entries: Vec<IndexEntry>,
            frees: Vec<Extent>,
            // This contains a list of entries that will be used to update the index cache. These
            // may originate from new updates (i.e. from the pending changes list), or from disk
            // location changes (i.e. from a block allocator rebalance operation).
            cache_updates: Vec<IndexEntry>,
            obsoleted: AtimeHistogramPhys,
            timer: Instant,
        }

        enum IngestSource {
            Index,
            PendingChange,
        }

        impl Progress {
            fn new(tx: mpsc::Sender<IndexMessage>, first_ghost: Atime, first_live: Atime) -> Self {
                Self {
                    tx,
                    last_key: None,
                    entries: Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                    frees: Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                    cache_updates: Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                    obsoleted: AtimeHistogramPhys::new(first_ghost, first_live),
                    timer: Instant::now(),
                }
            }

            /// As entries from the old index are processed (possibly added to the new index),
            /// they are now "obsolete" in the old index, so need to be removed from the atime
            /// histogram.
            fn obsolete(&mut self, entry: IndexEntry) {
                self.obsoleted.insert(entry.value);
            }

            /// When an old index entry already exists for a newly inserted key, the new entry will
            /// replace the old, so "evict" the old entry: if the entry is a ghost, then there is
            /// nothing to do, otherwise, add the entry to the free list.
            async fn evict(&mut self, state: &MergeState, entry: IndexEntry) {
                if let Some(extent) = entry.value.extent() {
                    match &state.rebalance {
                        Some(rebalance) => {
                            // If remap() is None, the data was evicted by the rebalance, so there's
                            // nothing to free here.
                            if let Some(location) = rebalance.remap(extent) {
                                self.frees.push(Extent {
                                    location,
                                    size: extent.size,
                                });
                            }
                        }
                        None => self.frees.push(extent),
                    }

                    if self.entries.len() >= *MERGE_PROGRESS_CHUNK
                        || self.frees.len() >= *MERGE_PROGRESS_CHUNK
                        || self.cache_updates.len() >= *MERGE_PROGRESS_CHUNK
                    {
                        self.report().await;
                    }
                }
            }

            /// The provided index entry is either:
            /// 1. Added to the list of entries to be part of the new index, or
            /// 2. Added to the list of entries to be evicted from the cache, or
            /// 3. Dropped because it is an already evicted entry that is no longer being tracked.
            async fn ingest(
                &mut self,
                state: &MergeState,
                mut entry: IndexEntry,
                source: IngestSource,
            ) {
                if let Some(extent) = entry.value.extent() {
                    if let Some(rebalance) = &state.rebalance {
                        let remapped_location = rebalance.remap(extent);
                        if entry.value.location() != remapped_location {
                            // The data for this entry has been moved due to a cache rebalance
                            // operation. Update the entry using the new location for the data.
                            // Note: if rebalance was unable to move the data (evicting the entry
                            // instead) the new location will be None.
                            entry.value.set_location(remapped_location);
                            self.cache_updates.push(entry);
                        }
                    }
                }
                if entry.value.atime() >= state.eviction_cutoff {
                    // If this entry was evicted during rebalance, don't put it in the new index
                    if entry.value.location().is_some() {
                        self.entries.push(entry);

                        if matches!(source, IngestSource::PendingChange) {
                            self.cache_updates.push(entry);
                        }
                    }
                } else {
                    if let Some(extent) = entry.value.extent() {
                        // This is a new ghost entry, free and strip old location infomation
                        self.frees.push(extent);
                        entry.value.set_location(None);
                        state.stats.track_count(Evictions);
                    }
                    if entry.value.atime() >= state.ghost_cutoff {
                        // Preserve ghost entry for our ghost history
                        self.entries.push(entry);
                    }
                }
                self.last_key = Some(entry.key);

                if self.entries.len() >= *MERGE_PROGRESS_CHUNK
                    || self.frees.len() >= *MERGE_PROGRESS_CHUNK
                    || self.cache_updates.len() >= *MERGE_PROGRESS_CHUNK
                {
                    self.report().await;
                }
            }

            /// Send a message to the next_index_task, with the current set of index entries to
            /// add and the current set of freed entries. Note: if we don't have a "last_key"
            /// then there is nothing to send.
            async fn report(&mut self) {
                if let Some(last_key) = self.last_key {
                    self.tx
                        .send(IndexMessage {
                            last_key,
                            entries: mem::replace(
                                &mut self.entries,
                                Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                            ),
                            frees: mem::replace(
                                &mut self.frees,
                                Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                            ),
                            cache_updates: mem::replace(
                                &mut self.cache_updates,
                                Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                            ),
                            obsoleted: self.obsoleted.take(),
                        })
                        .await
                        .unwrap_or_else(|e| panic!("couldn't send: {}", e));
                    trace!(
                        "Collected and sent {} entries and {} frees to next_index_task in {}ms",
                        self.entries.len(),
                        self.frees.len(),
                        self.timer.elapsed().as_millis()
                    );
                    self.timer = Instant::now();
                } else {
                    assert!(
                        self.entries.is_empty(),
                        "found {} entries, expected 0",
                        self.entries.len()
                    );
                    assert!(
                        self.frees.is_empty(),
                        "found {} frees, expected 0",
                        self.frees.len()
                    );
                }
            }
        }

        debug!("using {:?} as start key for merge", start_key);
        let mut index_stream;
        let mut progress;
        {
            let old_index = old_index_lock.read().await;
            info!(
                "writing new index to merge {} pending changes into index of {} entries ({}), eviction cutoff {:?}, ghost cutoff {:?}",
                self.old_pending_changes.len(),
                old_index.len(),
                nice_p2size(old_index.num_bytes()),
                self.eviction_cutoff,
                self.ghost_cutoff,
            );
            index_stream = old_index.iter_chunks();
            progress = Progress::new(
                tx,
                old_index.first_ghost_atime(),
                old_index.first_live_atime(),
            );
        }
        let mut pending_changes_iter = self
            .old_pending_changes
            .range((start_key.map_or(Unbounded, Excluded), Unbounded))
            .peekable();

        let mut index_skips: u64 = 0;
        while let Some(chunk) = index_stream.next().await {
            for &entry in chunk.entries() {
                // If the next index is already "started", advance the old index to the start point
                // XXX - would be nice to simply *start* from the start_key, rather than iterate up
                // to it
                if let Some(start_key) = start_key {
                    if entry.key <= start_key {
                        super_trace!("skipping index entry: {:?}", entry.key);
                        index_skips += 1;
                        continue;
                    }
                }

                // First, process any pending changes which are before this
                // index entry, which must all be Inserts (AtimeUpdates refer
                // to existing Index entries).
                while let Some((&pc_key, &PendingChange::Insert(pc_value))) =
                    pending_changes_iter.peek()
                {
                    if pc_key >= entry.key {
                        break;
                    }
                    // Add this new entry to the index
                    progress
                        .ingest(
                            self,
                            IndexEntry {
                                key: pc_key,
                                value: pc_value,
                            },
                            IngestSource::PendingChange,
                        )
                        .await;
                    pending_changes_iter.next();
                }

                progress.obsolete(entry);

                let next_pc_opt = pending_changes_iter.peek();
                match next_pc_opt {
                    Some((&pc_key, &PendingChange::Insert(pc_value))) => {
                        // Most insertions are processed above. However, if there is an index
                        // entry with the same key then we are replacing an entry. This may
                        // be a ghost entry being recached or perhaps a heal() of a bad entry.
                        if pc_key == entry.key {
                            // Replace the index entry with the newly inserted entry.
                            if entry.value.location().is_some() {
                                debug!("Insert of {:?} replaces {:?}", pc_value, entry);
                            }
                            progress.evict(self, entry).await;
                            progress
                                .ingest(
                                    self,
                                    IndexEntry {
                                        key: pc_key,
                                        value: pc_value,
                                    },
                                    IngestSource::PendingChange,
                                )
                                .await;
                            // this pending change is consumed
                            pending_changes_iter.next();
                        } else {
                            assert_gt!(pc_key, entry.key);
                            progress.ingest(self, entry, IngestSource::Index).await;
                        }
                    }
                    Some((&pc_key, &PendingChange::UpdateAtime(UpdateAtime(pc_value, _)))) => {
                        if pc_key == entry.key {
                            // Update this entry with the new atime from the pending change
                            assert_eq!(pc_value.extent(), entry.value.extent());
                            progress
                                .ingest(
                                    self,
                                    IndexEntry {
                                        key: pc_key,
                                        value: pc_value,
                                    },
                                    IngestSource::PendingChange,
                                )
                                .await;

                            // this pending change is consumed
                            pending_changes_iter.next();
                        } else {
                            // We shouldn't have skipped any, because there has to be a
                            // corresponding Index entry
                            assert_gt!(pc_key, entry.key);
                            progress.ingest(self, entry, IngestSource::Index).await;
                        }
                    }
                    None => {
                        // no more pending changes
                        progress.ingest(self, entry, IngestSource::Index).await;
                    }
                }
            }
        }
        while let Some((&pc_key, &PendingChange::Insert(pc_value))) = pending_changes_iter.peek() {
            // Add this new entry to the index
            progress
                .ingest(
                    self,
                    IndexEntry {
                        key: pc_key,
                        value: pc_value,
                    },
                    IngestSource::PendingChange,
                )
                .await;
            // Consume pending change.  We don't do that in the `while let`
            // because we want to leave any unmatched items in the iterator so
            // that we can print them out when failing below.
            pending_changes_iter.next();
        }
        // Other pending changes refer to existing index entries and therefore should have been
        // processed above
        assert!(
            pending_changes_iter.peek().is_none(),
            "next={:?}",
            pending_changes_iter.peek().unwrap()
        );
        debug!("skipped {} index entries", index_skips);

        // Send final progress message with final list content
        progress.report().await;

        info!(
            "merge task completed in {:.1}s",
            begin.elapsed().as_secs_f64(),
        );
    }

    async fn rebalance(&self, block_access: &BlockAccess) {
        let map = match self.rebalance.as_ref() {
            None => return,
            Some(rebalance) => &rebalance.map,
        };

        let begin = Instant::now();

        info!("starting rebalance with {} entries", map.len());

        futures::stream::iter(map)
            .for_each_concurrent(
                *CACHE_REBALANCE_CONCURRENCY_LIMIT * block_access.disks().count(),
                |(old, maybe_new)| async move {
                    if let Some(new) = maybe_new {
                        let bytes = block_access
                            .read_raw(*old, DiskIoType::MaintenanceRead)
                            .await;
                        block_access
                            .write_raw(*new, bytes, DiskIoType::MaintenanceWrite)
                            .await;
                    }
                },
            )
            .await;

        let bytes_copied = map.iter().map(|(old, _)| old.size).sum::<u64>();

        info!(
            "took {}ms for rebalance to copy {} ({:.1}MB/s)",
            begin.elapsed().as_millis(),
            nice_p2size(bytes_copied),
            (bytes_copied as f64 / 1024f64 / 1024f64) / begin.elapsed().as_secs_f64(),
        );
    }
}

type PendingChanges = BTreeMap<IndexKey, PendingChange>;

struct ZettaCacheState {
    block_access: Arc<BlockAccess>,
    primary: PrimaryPhys,
    guid: u64,
    primary_disk: DiskId,
    block_allocator: BlockAllocator,
    pending_changes: PendingChanges,
    pending_changes_trigger: usize,
    pending_changes_cap: usize,
    // Keep state associated with any on-going merge here
    merge: Option<Arc<MergeState>>,
    index_cache: LruCache<IndexKey, IndexValue>,
    slab_allocator: Arc<SlabAllocator>,
    // includes pending_changes, including AtimeUpdate which is not logged
    atime_histogram: AtimeHistogram,
    size_histogram: SizeHistogramPhys,
    // XXX move this to its own file/struct with methods to load, etc?
    operation_log: BlockBasedLog<OperationLogEntry>,
    // This is needed to ensure that reads complete before we complete the next
    // checkpoint, so that we don't overwrite their locations on disk (if the
    // block is evicted and freed from the cache).
    outstanding_reads: ConcurrentBatch,
    // This is needed to ensure that writes complete before we complete the next
    // checkpoint, so that they are persisted to disk.
    outstanding_writes: ConcurrentBatch,

    atime: Atime,
    stats: Arc<CacheStats>,
}

pub struct LockedKey(LockedItem<IndexKey>);

impl LockedKey {
    fn key(&self) -> IndexKey {
        *self.0.value()
    }
}

pub enum LookupResponse {
    Present(AlignedBytes, LockedKey),
    Absent(LockedKey),
}

#[derive(Clone, Copy, Debug)]
pub enum InsertSource {
    Heal,
    Read,
    SpeculativeRead,
    Write,
}

impl ZettaCache {
    async fn create(block_access: &BlockAccess) {
        let guid: u64 = rand::random();

        let total_capacity = block_access.total_capacity();
        info!("creating cache from {} disks", block_access.disks().count());

        let new_capacity = block_access
            .disks()
            .map(|disk| {
                Extent::new(
                    disk,
                    SUPERBLOCK_SIZE,
                    block_access.disk_size(disk) - SUPERBLOCK_SIZE,
                )
            })
            .collect::<Vec<_>>();

        let checkpoint = CheckpointPhys {
            id: CheckpointId(0),
            pool_guids: Default::default(),
            block_allocator: BlockAllocatorPhys::new(block_access),
            slab_allocator: SlabAllocatorPhys::new(new_capacity),
            old_index: IndexRunPhys::new(Atime(0), Atime(0)),
            operation_log: Default::default(),
            last_atime: Atime(0),
            size_histogram: SizeHistogramPhys::new(
                total_capacity + GHOST_CACHE_SIZE_PCT.0.apply(total_capacity),
                total_capacity,
                RESERVED_SLABS_PCT.apply(total_capacity),
                *QUANTILES_IN_SIZE_HISTOGRAM,
            ),

            merge_progress: None,
        };
        let slab_allocator = SlabAllocatorBuilder::new(checkpoint.slab_allocator.clone()).build();
        let checkpoint_extents = checkpoint.write(block_access, &slab_allocator).await;
        PrimaryPhys::new(
            block_access
                .disks()
                .map(|disk| (disk, DiskPhys::new(block_access.disk_size(disk))))
                .collect(),
            checkpoint_extents,
        )
        .write_all(DiskId::new(0), guid, block_access)
        .await;
    }

    fn index_cache_estimate_capacity(system_memory: usize) -> usize {
        // Calculate the maximum size for the index cache as a percentage of system memory
        let target_index_cache_bytes = INDEX_CACHE_ENTRIES_MEM_PCT.apply(system_memory);

        // Looking at the source of LruCache at the time of this writing we see that LruEntry<K,V>
        // is composed of the following elements: K, V, and 2 pointers. Thus, we use the following
        // formula to approximate the size of each entry in the index cache which empirically seem
        // to be fairly accurate:
        let index_cache_entry_size =
            mem::size_of::<IndexKey>() + mem::size_of::<IndexValue>() + 2 * mem::size_of::<usize>();

        // Even when the cache is empty LruCache pre-allocates buckets inducing an overhead that is
        // separate from the actual per entry overhead yet tied to the number of entries that it can
        // hold.  The cache overhead consists of a tiny constant overhead for some of its metadata
        // tracking (e.g. capacity, hasher fields, etc..) and per-entry overhead. At the time of
        // this writing the LruCache uses a KeyRef<K> (8 bytes) for the key, and a
        // Box<LruEntry> (8 bytes) as the value. Additionally assuming that HashBrown is
        // used as the underlying HashMap we expect 8 + 1 bytes of overhead per entry. That
        // would imply that the overhead be close to 3 * sizeof(usize) per entry but
        // empirically we've found that it is closer to 5 * sizeof(usize).
        let index_cache_overhead_bytes = 5 * mem::size_of::<usize>();

        let index_cache_cap =
            target_index_cache_bytes / (index_cache_entry_size + index_cache_overhead_bytes);

        info!(
            "index-cache target_size: {}/{} breakdown: [{} for {} entries of {}] + [{} of cache state overhead]",
            nice_p2size(target_index_cache_bytes as u64),
            nice_p2size(system_memory as u64),
            nice_p2size((index_cache_cap * index_cache_entry_size) as u64),
            index_cache_cap,
            nice_p2size(index_cache_entry_size as u64),
            nice_p2size((index_cache_overhead_bytes * index_cache_cap) as u64)
        );
        index_cache_cap
    }

    pub async fn open(paths: Vec<&str>, clear_incompatible_cache: bool) -> Result<Self> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in paths {
            disks.push(Disk::new(path, false)?);
        }
        let block_access = Arc::new(BlockAccess::new(disks, false));

        let feature_flags = match PrimaryPhys::read_features(&block_access).await {
            Ok(feature_flags) => feature_flags,
            Err(_) => {
                // XXX need proper create CLI
                Self::create(&block_access).await;
                PrimaryPhys::read_features(&block_access).await.unwrap()
            }
        };
        if let Err(feature_error) = check_features(&feature_flags) {
            if !clear_incompatible_cache {
                error!("{}", feature_error);
                return Err(anyhow!("{}", feature_error));
            }
            info!("Resetting cache - {}", feature_error);
            Self::create(&block_access).await;
            let features = PrimaryPhys::read_features(&block_access).await.unwrap();
            assert!(check_features(&features).is_ok());
        }

        let (mut primary, primary_disk, guid, extra_disks) =
            PrimaryPhys::read(&block_access).await.unwrap();

        // XXX proper error handling
        let mut checkpoint = CheckpointPhys::read(&block_access, &primary.checkpoint).await;
        assert_eq!(checkpoint.id, primary.checkpoint_id);

        let mut size_changed = false;

        // XXX proper CLI for adding disks
        if !extra_disks.is_empty() {
            info!(
                "adding {} disks to existing {}-disk cache",
                extra_disks.len(),
                primary.disks.len()
            );

            let new_capacity = extra_disks
                .iter()
                .map(|&disk| {
                    Extent::new(
                        disk,
                        SUPERBLOCK_SIZE,
                        block_access.disk_size(disk) - SUPERBLOCK_SIZE,
                    )
                })
                .collect::<Vec<_>>();
            primary.disks.extend(
                extra_disks
                    .iter()
                    .map(|&disk| (disk, DiskPhys::new(block_access.disk_size(disk)))),
            );

            checkpoint.slab_allocator.extend(new_capacity);
            size_changed = true;
        }

        let expanded_capacity = primary
            .disks
            .iter()
            .filter_map(|(&disk, phys)| {
                let new_size = block_access.disk_size(disk);
                if new_size > phys.size {
                    let added_bytes = new_size - phys.size;
                    Some(Extent::new(disk, phys.size, added_bytes))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if !expanded_capacity.is_empty() {
            info!("expanding existing disks: {:?}", expanded_capacity);
            checkpoint.slab_allocator.extend(expanded_capacity);

            // Update disk size in primary
            for (&disk, phys) in &mut primary.disks {
                phys.size = block_access.disk_size(disk);
            }
            size_changed = true;
        }

        info!(
            "opening ZettaCache {} with {} disks",
            guid,
            primary.disks.len()
        );

        let mut slab_builder = SlabAllocatorBuilder::new(checkpoint.slab_allocator.clone());
        for &extent in &primary.checkpoint {
            slab_builder.claim(slab_builder.extent_to_slab_id(extent));
        }
        checkpoint.claim(&mut slab_builder);

        let block_builder = BlockAllocatorBuilder::new(
            block_access.clone(),
            &mut slab_builder,
            checkpoint.block_allocator,
        )
        .await;

        let slab_allocator = Arc::new(slab_builder.build());
        let block_allocator = block_builder.build(slab_allocator.clone()).await;

        let operation_log = BlockBasedLog::open(
            block_access.clone(),
            slab_allocator.clone(),
            checkpoint.operation_log,
        );

        let old_index = IndexRun::open(
            block_access.clone(),
            slab_allocator.clone(),
            checkpoint.old_index,
        )
        .await;

        // Note, the old_index histogram covers only the part that doesn't overlap with the
        // new_index.
        let mut atime_histogram_phys = old_index.atime_histogram().clone();
        if let Some(merge_progress) = &checkpoint.merge_progress {
            assert_eq!(old_index.trim_key(), merge_progress.new_index.last_key());
            atime_histogram_phys += merge_progress.new_index.atime_histogram();
        }

        let (old_pending_changes, new_index) = match &checkpoint.merge_progress {
            Some(merge_progress) => {
                let old_operation_log = BlockBasedLog::open(
                    block_access.clone(),
                    slab_allocator.clone(),
                    merge_progress.operation_log.clone(),
                );

                (
                    Some(
                        Self::load_operation_log(&old_operation_log, &mut atime_histogram_phys)
                            .await,
                    ),
                    Some(
                        ReadOnlyIndexRun::open(
                            block_access.clone(),
                            slab_allocator.clone(),
                            merge_progress.new_index.clone(),
                        )
                        .await,
                    ),
                )
            }
            None => (None, None),
        };

        let mut sysinfo = System::new();
        sysinfo.refresh_system();
        let system_memory = usize::from64(sysinfo.total_memory() * 1024);

        // Calculate a maximum size for the pending_changes as a percentage of system memory
        // Note that during a merge, this space must also accomodate the space used by
        // old_pending_changes
        let pending_changes_max_bytes = PENDING_CHANGES_MEM_PCT.apply(system_memory);
        // The BTreeMap type has about a 35% overhead, so we have a 65% usable capacity for data
        // entries
        let pending_changes_entries_bytes = pending_changes_max_bytes * 65 / 100;
        // Each entry in the BTreeMap is comprised of a key (IndexKey) and a value (PendingChange)
        let pending_changes_entry_size =
            mem::size_of::<IndexKey>() + mem::size_of::<PendingChange>();
        // Limit the number of pending change entries to not exceed the amount of memory being made
        // available
        let pending_changes_cap = pending_changes_entries_bytes / pending_changes_entry_size;
        // In order to stay inside this desired cap, we need to be triggering a new merge before we
        // are more than half way to the cap. Trigger at about 1/3 to provide some slop
        // space.
        let pending_changes_trigger = pending_changes_cap / 3;
        info!(
            "pending changes max length set to {} entries [{}% of {} = {} and entry size {}]",
            pending_changes_cap,
            PENDING_CHANGES_MEM_PCT.as_percent(),
            nice_p2size(system_memory as u64),
            nice_p2size(pending_changes_entries_bytes as u64),
            nice_p2size(pending_changes_entry_size as u64)
        );

        let pending_changes =
            Self::load_operation_log(&operation_log, &mut atime_histogram_phys).await;
        debug!("atime_histogram: {:#?}", atime_histogram_phys,);

        let stats = Arc::new(CacheStats::default());

        let mut state = ZettaCacheState {
            block_access: block_access.clone(),
            pending_changes,
            pending_changes_cap,
            pending_changes_trigger,
            merge: None,
            index_cache: with_alloctag("ZettaCacheState::index_cache hashtable", || {
                LruCache::new(Self::index_cache_estimate_capacity(system_memory))
            }),
            atime_histogram: AtimeHistogram::new(atime_histogram_phys),
            size_histogram: checkpoint.size_histogram,
            operation_log,
            primary,
            primary_disk,
            guid,
            outstanding_reads: Default::default(),
            outstanding_writes: Default::default(),
            atime: checkpoint.last_atime,
            block_allocator,
            slab_allocator,
            stats: stats.clone(),
        };

        // Now that BlockAllocator is open grab its size stats (these will be updated periodically)
        state.update_stats();

        if size_changed {
            // The hit data isn't accurate across cache size changes, so clear
            // it, which also updates the histogram parameters to reflect the
            // new cache size.
            state.clear_hit_data();
        }

        let this = Self(Arc::new(Inner {
            slab_allocator: state.slab_allocator.clone(),
            old_index: Arc::new(tokio::sync::RwLock::new(old_index)),
            new_index: Arc::new(tokio::sync::RwLock::new(new_index)),
            state: Arc::new(tokio::sync::Mutex::new(state)),
            outstanding_lookups: LockSet::new(),
            demand_buffer_bytes_available: Arc::new(Semaphore::new(usize::from64(
                CACHE_INSERT_DEMAND_BUFFER_SIZE.as_u64(),
            ))),
            speculative_buffer_bytes_available: Arc::new(Semaphore::new(usize::from64(
                CACHE_INSERT_SPECULATIVE_BUFFER_SIZE.as_u64(),
            ))),
            block_access,
            stats,
            timebase: Instant::now(),
            cache_runtime_id: Uuid::new_v4(),
            pool_guids: PoolGuidMapping::open(checkpoint.pool_guids),
        }));

        let merging = match checkpoint.merge_progress {
            Some(progress) => Some(
                this.state
                    .lock()
                    .await
                    .resume_merge_task(
                        this.old_index.clone(),
                        old_pending_changes.unwrap(),
                        progress,
                    )
                    .await,
            ),
            None => None,
        };

        let my_cache = this.clone();
        measure!("checkpoint_task").spawn(async move {
            my_cache.checkpoint_task(merging).await;
        });

        let state = this.state.clone();
        measure!("atime interval").spawn(async move {
            // XXX maybe we should bump the atime after a set number of
            // accesses, so each histogram bucket starts with the same count.
            // We could then add an auxiliary structure saying what wall clock
            // time each atime value corresponds to.
            let mut interval = tokio::time::interval(*ATIME_INTERVAL);
            loop {
                interval.tick().await;
                let mut state = state.lock().await;
                state.atime = state.atime.next();
            }
        });

        let state = this.state.clone();
        let cache = this.clone();
        measure!("stats interval").spawn(async move {
            let mut interval = tokio::time::interval(*STATS_INTERVAL);
            loop {
                interval.tick().await;

                cache.stats.track_instantaneous(
                    SpeculativeBufferBytesAvailable,
                    CACHE_INSERT_SPECULATIVE_BUFFER_SIZE.as_u64()
                        - cache.speculative_buffer_bytes_available.available_permits() as u64,
                );
                cache.stats.track_instantaneous(
                    DemandBufferBytesAvailable,
                    CACHE_INSERT_DEMAND_BUFFER_SIZE.as_u64()
                        - cache.demand_buffer_bytes_available.available_permits() as u64,
                );
                measure!().fut(lock_non_send(&state)).await.update_stats();
            }
        });

        Ok(this)
    }

    /// Load the provided operation log to produce a new pending changes map.
    /// Update the atime_histogram with the data from the pending changes.
    async fn load_operation_log(
        operation_log: &BlockBasedLog<OperationLogEntry>,
        atime_histogram: &mut AtimeHistogramPhys,
    ) -> PendingChanges {
        let begin = Instant::now();
        let mut num_insert_entries: u64 = 0;
        let mut pending_changes = BTreeMap::new();
        operation_log
            .iter()
            .for_each(|entry| {
                match entry {
                    OperationLogEntry::Insert(key, value) => {
                        if let Some(PendingChange::Insert(old_value)) =
                            pending_changes.insert(key, PendingChange::Insert(value))
                        {
                            // We are replacing an old value, adjust the histogram to reflect the
                            // change
                            atime_histogram.remove(old_value);
                        }
                        super_trace!("insert {:?} {:?}", key, value);
                        num_insert_entries += 1;
                        atime_histogram.insert(value);
                    }
                };
                future::ready(())
            })
            .await;
        info!(
            "loaded operation_log from {} inserts into {} pending_changes in {}ms",
            num_insert_entries,
            pending_changes.len(),
            begin.elapsed().as_millis()
        );
        pending_changes
    }

    /// The checkpoint task is primarily responsible for writing out a persistent checkpoint every
    /// 60s. It is also responsible for kicking off a merge task every time we accumulate enough
    /// pending change. While a merge task is running, this task listens for and processes
    /// eviction requests from the merge task. The active merge task state is also captured in
    /// each checkpoint so that it may be resumed from the checkpoint if necessary. On resume
    /// the merge task is restarted during cache open and a channel to task and the index phys
    /// for the current progress are passed in.
    async fn checkpoint_task(
        &self,
        mut merging: Option<(mpsc::Receiver<MergeMessage>, IndexRunPhys)>,
    ) {
        let mut next_tick = tokio::time::Instant::now();
        let mut completed_merge = false;
        loop {
            // if there is no current merging state, check to see if a merge should be started
            {
                let mut state = self.state.lock().await;
                if state.merge.is_none() {
                    assert!(merging.is_none());
                    merging = state.try_start_merge_task(self.old_index.clone()).await;
                }
            }
            if let Some((rx, new_index_phys)) = &mut merging {
                let begin = Instant::now();
                let mut msg_count: u64 = 0;
                let mut free_count = 0;
                let mut cache_updates_count = 0;
                let mut state_lock_held = Duration::ZERO;
                // we have a channel to an active merge task, check it for messages
                loop {
                    let result = timeout_at(next_tick, rx.recv()).await;
                    match result {
                        // capture merge progress: the current next index phys and eviction requests
                        Ok(Some(MergeMessage::Progress(progress))) => {
                            msg_count += 1;
                            free_count += progress.frees.len();
                            cache_updates_count += progress.cache_updates.len();

                            trace!(
                                "merge message with {} frees and {} cache updates.",
                                progress.frees.len(),
                                progress.cache_updates.len(),
                            );

                            super_trace!("eviction requested for {:?}", progress.frees);
                            super_trace!(
                                "cache updates requested for {:?}",
                                progress.cache_updates
                            );

                            {
                                let mut state = self.state.lock().await;
                                let begin = Instant::now();

                                // free the extent ranges associated with the evicted blocks
                                for extent in progress.frees {
                                    state.block_allocator.free(extent);
                                }

                                // Here is where we populate the index cache to contain any new
                                // keys that may have been inserted or updated, as well as ensure
                                // any existing keys are consistent w.r.t. a remap (i.e. ensuring
                                // the cache references the key's new/remapped location on disk).
                                //
                                // Note that keys that are already in the cache will have their
                                // associated values updated, and keys that are not will be added
                                // (with their values). Also note that any keys with no associated
                                // values (ghost keys) will be removed from the index. This is
                                // important for keys which may have been "valid", but were evicted
                                // because they could not be remapped.
                                for entry in progress.cache_updates.into_iter() {
                                    match entry.value.location() {
                                        // It's possible the key wasn't already in the cache, so
                                        // this may add or update the key.
                                        Some(_) => state.index_cache.put(entry.key, entry.value),
                                        // It's possible the key isn't in the cache; .pop() doesn't
                                        // fail in that case.
                                        None => state.index_cache.pop(&entry.key),
                                    };
                                }

                                state_lock_held += begin.elapsed();
                            } // drop state lock

                            *new_index_phys = progress.new_index;
                            let mut old_index = self.old_index.write().await;
                            let mut new_index_opt = self.new_index.write().await;
                            match &mut *new_index_opt {
                                Some(new_index) => {
                                    new_index.update(new_index_phys.clone(), &progress.index_delta);
                                }
                                None => {
                                    *new_index_opt = Some(
                                        ReadOnlyIndexRun::open(
                                            self.block_access.clone(),
                                            self.slab_allocator.clone(),
                                            new_index_phys.clone(),
                                        )
                                        .await,
                                    );
                                }
                            }
                            if let Some(last_key) = new_index_phys.last_key() {
                                old_index.trim(last_key, &progress.obsoleted);
                            }
                        }
                        // merge task complete, replace the current index with the new index
                        Ok(Some(MergeMessage::Complete(new_index))) => {
                            let mut old_index = self.old_index.write().await;
                            let mut new_index_opt = self.new_index.write().await;

                            let mut state = self.state.lock().await;
                            state.rotate_index(&mut old_index, new_index).await;
                            state.block_allocator.rebalance_fini();
                            *new_index_opt = None;
                            merging = None;
                            completed_merge = true;
                            break;
                        }
                        Ok(None) => panic!("channel closed before Complete message received"),
                        Err(_) => break, // timed out
                    }
                }
                debug!(
                    "processed {} merge messages with {} frees and {} cache updates in {}ms (state lock held for {}ms)",
                    msg_count,
                    free_count,
                    cache_updates_count,
                    begin.elapsed().as_millis(),
                    state_lock_held.as_millis(),
                );
            }

            // flush out a new checkpoint every CHECKPOINT_INTERVAL to capture the current state
            sleep_until(next_tick).await;
            self.flush_checkpoint(
                merging.as_mut().map(|(_, phys)| (phys.clone())),
                completed_merge,
            )
            .await;
            next_tick = std::cmp::max(
                tokio::time::Instant::now(),
                next_tick + *CHECKPOINT_INTERVAL,
            );
            completed_merge = false;
        }
    }

    async fn flush_checkpoint(&self, new_index: Option<IndexRunPhys>, completed_merge: bool) {
        {
            // Wait for all outstanding reads, so that if we free the space they are reading, it
            // can't be overwritten until after the read completes.  We wait without holding the
            // state lock, and we replace the existing lock with a new one, so that new reads can
            // start even while we are waiting for the previous batch to complete.
            let begin = Instant::now();
            // Bind to a variable here so that we can drop the state lock before waiting for the
            // batch of reads to complete.
            let outstanding_reads = lock_non_send(&self.state).await.outstanding_reads.rotate();
            outstanding_reads.await;
            debug!(
                "waited for outstanding_reads in {}ms",
                begin.elapsed().as_millis()
            );
        }

        {
            // Wait for all outstanding writes, so that if we crash, the blocks referenced by the
            // index/operation_log will actually have the correct contents.  See above comments
            // on how the ConcurrentBatch is manipulated.
            let begin = Instant::now();
            let outstanding_writes = lock_non_send(&self.state).await.outstanding_writes.rotate();
            outstanding_writes.await;
            debug!(
                "waited for outstanding_writes in {}ms",
                begin.elapsed().as_millis()
            );
        }

        let (old_index_phys, delta) = self.old_index.write().await.flush().await;
        assert!(delta.is_empty());
        let mut state = self.state.lock().await;

        // Now that we have the state lock, we need to wait for outstanding i/os again, because
        // more i/os could have been initiated while we were waiting above.  Those i/os will
        // become part of this checkpoint, so we have to wait for them.
        {
            let begin = Instant::now();
            state.outstanding_reads.rotate().await;
            debug!(
                "waited for outstanding_reads with lock held in {}ms",
                begin.elapsed().as_millis()
            );
        }

        {
            let begin = Instant::now();
            state.outstanding_writes.rotate().await;
            debug!(
                "waited for outstanding_writes with lock held in {}ms",
                begin.elapsed().as_millis()
            );
        }

        state
            .flush_checkpoint(
                old_index_phys,
                new_index,
                completed_merge,
                self.pool_guids.to_phys(),
            )
            .await;
    }

    /// Look up this block in the zettacache, without updating its LRU order (i.e. its atime).
    /// No on-disk state will change as a result of this call.
    pub async fn peek(&self, guid: PoolGuid, block: BlockId) -> LookupResponse {
        let key = IndexKey::new(self.pool_guids.map_pool_guid(guid), block);
        let locked_key = LockedKey(measure!().fut(self.outstanding_lookups.lock(key)).await);

        let bytes = self
            .lookup_impl(&locked_key, true, false, |state, value| match value {
                Some(value) => state.peek(&locked_key, value).left_future(),
                None => future::ready(None).right_future(),
            })
            .await;

        self.stats.track_count(Lookup);
        match bytes {
            Some(bytes) => {
                self.stats.track_bytes(CacheHitBytes, bytes.len() as u64);
                self.stats.track_count(CacheHit);
                LookupResponse::Present(bytes, locked_key)
            }
            None => LookupResponse::Absent(locked_key),
        }
    }

    pub async fn lookup(&self, guid: PoolGuid, block: BlockId) -> LookupResponse {
        let key = IndexKey::new(self.pool_guids.map_pool_guid(guid), block);
        let locked_key = LockedKey(measure!().fut(self.outstanding_lookups.lock(key)).await);

        // In debug mode, return failure randomly every specified number of requests
        if *LOOKUP_FAIL_RANDOM != 0 && rand::thread_rng().gen_ratio(1, *LOOKUP_FAIL_RANDOM) {
            return LookupResponse::Absent(locked_key);
        }

        let bytes = self
            .lookup_impl(&locked_key, true, true, |state, value| {
                state.size_histogram.lookup();
                match value {
                    Some(value) => state.lookup(&locked_key, value).left_future(),
                    None => future::ready(None).right_future(),
                }
            })
            .await;

        self.stats.track_count(Lookup);
        match bytes {
            Some(bytes) => {
                super_trace!("cache hit for {:?}", key);
                self.stats.track_bytes(CacheHitBytes, bytes.len() as u64);
                self.stats.track_count(CacheHit);
                LookupResponse::Present(bytes, locked_key)
            }
            None => LookupResponse::Absent(locked_key),
        }
    }

    async fn lookup_impl<F, R, Fut>(
        &self,
        locked_key: &LockedKey,
        count_index_hits: bool,
        count_ghost_hits: bool,
        f: F,
    ) -> R
    where
        F: FnOnce(&mut ZettaCacheState, Option<ValidIndexValue>) -> Fut,
        Fut: Future<Output = R> + Send,
    {
        let key = locked_key.key();
        // Hold the index lock over the whole operation so that the index can't change after we
        // get the value from it.  Lock ordering requires that we lock the index before locking
        // the state.
        let old_index_guard = self.old_index.read().await;
        let new_index_guard = self.new_index.read().await;

        let fut_or_f = {
            // We don't want to hold the state lock while reading from disk so we use
            // lock_non_send() to ensure that we can't hold it across .await.
            let mut state = measure!().fut(lock_non_send(&self.state)).await;

            let got_value = |state: &mut ZettaCacheState, f: F, counter, value| {
                if count_ghost_hits {
                    state.ghost_hit_check(value);
                }
                if count_index_hits {
                    self.stats.track_count(counter);
                }
                let validated = state.validate(value);
                Either::Left(f(state, validated))
            };

            match state.pending_changes.get(&key).copied() {
                Some(pc) => match pc {
                    PendingChange::Insert(value)
                    | PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                        got_value(&mut state, f, IndexHitPendingChanges, value)
                    }
                },
                None => {
                    if let Some(ms) = &state.merge {
                        if let Some(pc) = ms.old_pending_changes.get(&key).copied() {
                            match pc {
                                PendingChange::Insert(value)
                                | PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                                    got_value(&mut state, f, IndexHitPendingChanges, value)
                                }
                            }
                        } else {
                            match state.index_cache.get(&key) {
                                Some(&value) => got_value(&mut state, f, IndexHitIndexCache, value),
                                None => Either::Right(f),
                            }
                        }
                    } else {
                        match state.index_cache.get(&key) {
                            Some(&value) => got_value(&mut state, f, IndexHitIndexCache, value),
                            None => Either::Right(f),
                        }
                    }
                }
            }
        };

        let f = match fut_or_f {
            Either::Left(fut) => {
                // Got the index entry from pending state or index cache and already called f().
                // Now that we've dropped the state lock, run the future that it returned.
                return measure!().fut(fut).await;
            }
            Either::Right(f) => f,
        };

        super_trace!("lookup {key:?}: no PendingChange, no index_cache; reading index");

        let mut index = Either::Left(&*old_index_guard);
        if let Some(new_index) = &*new_index_guard {
            if let Some(new_last_key) = new_index.last_key() {
                // Note, if equal then it's already been moved to the new index.
                if key <= new_last_key {
                    index = Either::Right(new_index);
                }
            }
        }
        let (entry_opt, chunk_cache_hit) = match index {
            Either::Left(index) => measure!().fut(index.lookup(key)).await,
            Either::Right(index) => measure!().fut(index.lookup(key)).await,
        };
        if count_index_hits {
            if chunk_cache_hit {
                self.stats.track_count(IndexHitChunkCache);
            } else {
                self.stats.track_count(IndexHitDisk);
            }
        }
        let fut = match entry_opt {
            Some(entry) => {
                // Again, we don't want to hold the state lock while reading from disk so we use
                // lock_non_send() to ensure that we can't hold it across .await.
                let mut state = measure!().fut(lock_non_send(&self.state)).await;

                // The LockedKey prevents an entry for this key from being inserted while we
                // weren't holding the state lock.
                assert!(state.pending_changes.get(&key).is_none());
                if count_ghost_hits {
                    state.ghost_hit_check(entry.value);
                }
                let validated = state.validate(entry.value);
                if validated.is_none() {
                    super_trace!("lookup {key:?}: cache miss after reading index, invalid entry");
                }
                f(&mut state, validated)
            }
            None => {
                // key not in index
                super_trace!("lookup {key:?}: cache miss after reading index");
                let mut state = measure!().fut(lock_non_send(&self.state)).await;
                f(&mut state, None)
            }
        };
        measure!().fut(fut).await
    }

    async fn reserve_buffer_space(
        &self,
        bytes: usize,
        source: InsertSource,
    ) -> Option<OwnedSemaphorePermit> {
        let (buffer, wait_insert) = match source {
            InsertSource::Heal | InsertSource::SpeculativeRead | InsertSource::Write => {
                (self.speculative_buffer_bytes_available.clone(), false)
            }
            InsertSource::Read => (
                self.demand_buffer_bytes_available.clone(),
                *CACHE_WAIT_INSERT,
            ),
        };

        if wait_insert {
            let permit = buffer
                .acquire_many_owned(u32::try_from(bytes).unwrap())
                .await
                .expect("error from acquire_many_owned");
            Some(permit)
        } else {
            // The permit should be dropped when the write to disk completes. It serves to limit the
            // number of insert()'s that we can buffer before dropping (ignoring)
            // insertion requests.
            match buffer.try_acquire_many_owned(u32::try_from(bytes).unwrap()) {
                Ok(permit) => Some(permit),
                Err(tokio::sync::TryAcquireError::NoPermits) => None,
                Err(e) => panic!("unexpected error from try_acquire_many_owned: {:?}", e),
            }
        }
    }

    /// Return a block with a specific pattern of "corruption"
    pub fn corruption(len: usize, alignment: usize) -> AlignedBytes {
        let mut vec = with_alloctag("corruption()", || {
            util::AlignedVec::with_capacity(len, alignment)
        });
        vec.extend_from_value(len, *CORRUPTION_FILL);
        vec.into()
    }

    /// Insert into the cache in the current checkpoint (allocate a block, add to
    /// pending_changes and outstanding_writes).
    async fn insert_impl(&self, locked_key: LockedKey, bytes: AlignedBytes, source: InsertSource) {
        let bytes = if CORRUPT_INSERT_PCT.as_fraction() != 0.0
            && rand::thread_rng().gen_bool(CORRUPT_INSERT_PCT.as_fraction())
        {
            // For debug, replace the input data with a known corruption pattern
            debug!("Injecting corrupt data for {:?}", locked_key.key());
            Self::corruption(bytes.len(), bytes.alignment())
        } else {
            bytes
        };
        let len = bytes.len() as u64;
        let fut = measure!()
            .fut(lock_non_send(&self.state))
            .await
            .insert(locked_key, bytes);
        match measure!().fut(fut).await {
            Ok(_) => {
                self.stats.track_bytes(InsertBytes, len);
                self.stats.track_count(match source {
                    InsertSource::Heal => InsertForHeal,
                    InsertSource::Read => InsertForRead,
                    InsertSource::SpeculativeRead => InsertForSpeculativeRead,
                    InsertSource::Write => InsertForWrite,
                });
            }
            Err(InsertError::Allocation) => {
                self.stats.track_count(InsertDropCacheFull);
            }
            Err(InsertError::PendingChanges) => {
                self.stats.track_count(InsertDropCacheFull);
            }
        }
    }

    /// Initiates insertion of this block; doesn't wait for the write to disk.  The `bytes_fn`
    /// closure returns the AlignedBytes to insert.  This is useful if it's expensive to compute
    /// (e.g. we need to memcpy() it), as we won't invoke it if the block is not actually
    /// inserted due to the insertion buffer being full.
    pub async fn insert<F: FnOnce() -> AlignedBytes>(
        &self,
        locked_key: LockedKey,
        bytes_len: usize,
        bytes_fn: F,
        source: InsertSource,
    ) {
        // This permit will be dropped when the write to disk completes.  It serves to limit the
        // number of insert()'s that we can buffer before dropping (ignoring) insertion requests.
        let insert_permit = match measure!()
            .fut(self.reserve_buffer_space(bytes_len, source))
            .await
        {
            Some(permit) => permit,
            None => {
                self.stats.track_count(InsertDropBufferFull);
                return;
            }
        };

        let bytes = bytes_fn();
        assert_eq!(bytes.len(), bytes_len);

        let cache = self.clone();
        measure!("ZettaCache::insert()").spawn(async move {
            cache.insert_impl(locked_key, bytes, source).await;
            // We want to hold onto the insert_permit until the write completes because it
            // represents the memory that's required to buffer this insertion, which isn't
            // released until the io completes.  Similarly, the write_permit (roughly) represents
            // the disks' capacity to perform i/o.
            drop(insert_permit);
        });
    }

    pub async fn insert_all(
        &self,
        guid: PoolGuid,
        blocks: &HashMap<BlockId, Bytes>,
        source: InsertSource,
    ) {
        let pool_id = self.pool_guids.map_pool_guid(guid);
        let insert_permit = match self
            .reserve_buffer_space(
                blocks.values().map(|bytes| bytes.len()).sum::<usize>(),
                source,
            )
            .await
        {
            Some(permit) => permit,
            None => {
                // Pretend that it's bytes so we can add many at once
                self.stats
                    .track_bytes(InsertDropBufferFull, blocks.len() as u64);
                return;
            }
        };

        let futures = FuturesUnordered::new();

        for (&block, bytes) in blocks.iter() {
            let cache = self.clone();
            let aligned_bytes = AlignedBytes::from((*bytes).clone());
            let fut = async move {
                let key = IndexKey::new(pool_id, block);
                let locked_key = LockedKey(cache.outstanding_lookups.lock(key).await);

                let present = match source {
                    // Since block contents can't logically change, writes are normally to
                    // BlockId's that the zettacache has never seen before, so we don't bother
                    // with the lookup.  It's unlikely, but there might be an entry for this
                    // BlockId if the system crashed or the pool was rewound.  In that case, this
                    // will logically overwrite it.  The double insertion will be resolved in the
                    // next merge.
                    InsertSource::Write => false,
                    _ => {
                        measure!()
                            .fut(cache.lookup_impl(&locked_key, false, false, |_, value| {
                                future::ready(value.is_some())
                            }))
                            .await
                    }
                };

                if !present {
                    cache.insert_impl(locked_key, aligned_bytes, source).await;
                }
            };
            with_alloctag_hf("ZettaCache::ingest_all FuturesUnordered.push()", || {
                futures.push(fut)
            });
        }
        measure!("ZettaCache::insert_all()").spawn(async move {
            futures.count().await;
            // We want to hold onto the insert_permit until the write completes because it
            // represents the memory that's required to buffer this insertion, which isn't
            // released until the io completes.
            drop(insert_permit);
        });
    }

    pub async fn heal(&self, guid: PoolGuid, block: BlockId, object_bytes: AlignedBytes) {
        if let LookupResponse::Present(cache_bytes, locked_key) = self.peek(guid, block).await {
            // We only need to do the heal when the bytes contained in the cache differ from the
            // bytes contained in the object store. The bytes contained in the object store are
            // always preferred over the bytes contained in the cache; we assume the bytes passed
            // were retrieved from the object store.
            if *cache_bytes != *object_bytes {
                self.stats.track_count(HealedBlocks);
                debug!("healing cache: {:?}", locked_key.key());
                // Note: this will result in a second insert for the same key in the index. This
                // will be resolved either in the insert code (if the first insert is in
                // pending_changes) or later during the next merge.
                self.insert(
                    locked_key,
                    object_bytes.len(),
                    || object_bytes,
                    InsertSource::Heal,
                )
                .await;
            }
        }
    }

    pub fn sector_size(&self) -> usize {
        self.block_access.round_up_to_sector(1)
    }

    pub async fn hits_by_size_data(&self) -> SizeHistogramPhys {
        self.state.lock().await.size_histogram.clone()
    }

    pub async fn clear_hit_data(&self) {
        self.state.lock().await.clear_hit_data();
    }

    pub fn devices_as_json(&self) -> String {
        serde_json::to_string(&self.block_access.list_devices()).unwrap()
    }

    pub fn io_stats_as_json(&self) -> String {
        self.block_access.io_stats_as_json(self.cache_runtime_id)
    }

    pub async fn stats_as_json(&self) -> String {
        let mut stats = CacheStats::clone(&self.stats);
        stats.cache_runtime_id = self.cache_runtime_id;
        stats.timestamp = self.timebase.elapsed();
        serde_json::to_string(&stats).unwrap()
    }
}

pub struct ValidIndexValue(IndexValue);

impl ValidIndexValue {
    pub fn extent(&self) -> Extent {
        self.0.extent().unwrap()
    }
}

enum InsertError {
    Allocation,
    PendingChanges,
}

impl ZettaCacheState {
    const PENDING_CHANGES_TAG: &'static str = "ZettaCacheState.pending_changes";
    /// Validates the passed in index value; returns the value back if it's still valid, or None.
    fn validate(&self, value: IndexValue) -> Option<ValidIndexValue> {
        let live_cutoff = match &self.merge {
            Some(ms) => ms.eviction_cutoff,
            None => self.atime_histogram.first_live(),
        };

        if value.atime() < live_cutoff {
            None
        } else {
            assert!(value.location().is_some());
            Some(ValidIndexValue(value))
        }
    }

    fn ghost_hit_check(&mut self, value: IndexValue) {
        let (live_cutoff, ghost_cutoff) = match &self.merge {
            Some(ms) => (ms.eviction_cutoff, ms.ghost_cutoff),
            None => (
                self.atime_histogram.first_live(),
                self.atime_histogram.first_ghost(),
            ),
        };

        if value.atime() >= ghost_cutoff && value.atime() < live_cutoff {
            // This is a hit in the ghost hit-by-size histogram
            self.size_histogram
                .ghost_hit(self.atime_histogram.size_at(value.atime()));
        }
    }

    /// Read the value from disk, without updating its LRU order (i.e. its atime).  No on-disk
    /// state will change as a result of this call.
    fn peek(
        &self,
        locked_key: &LockedKey,
        valid_value: ValidIndexValue,
    ) -> impl Future<Output = Option<AlignedBytes>> {
        super_trace!(
            "cache hit: reading {:?} from {:?}",
            locked_key.key(),
            valid_value.0
        );

        let read_permit = self.outstanding_reads.acquire();
        let block_access = self.block_access.clone();
        let key = locked_key.key();
        async move {
            let bytes = block_access
                .read_raw(valid_value.extent(), DiskIoType::ReadDataForLookup)
                .await;

            // It's now OK for a checkpoint to complete, potentially freeing this block.
            drop(read_permit);

            if CORRUPT_LOOKUP_PCT.as_fraction() != 0.0
                && rand::thread_rng().gen_bool(CORRUPT_LOOKUP_PCT.as_fraction())
            {
                debug!("Returning corrupt data for {key:?}");
                Some(ZettaCache::corruption(bytes.len(), bytes.alignment()))
            } else {
                Some(bytes)
            }
        }
    }

    fn lookup(
        &mut self,
        locked_key: &LockedKey,
        valid_value: ValidIndexValue,
    ) -> impl Future<Output = Option<AlignedBytes>> {
        let key = locked_key.key();
        let old_value = valid_value.0;
        let old_atime = old_value.atime();

        // Add an entry to the hit-by-size histogram
        let size = self.atime_histogram.size_at(old_atime);
        super_trace!("cache size {} at {:?}", size, old_atime);
        self.size_histogram.live_hit(size);

        assert_le!(old_atime, self.atime);
        let new_value = IndexValue::new(old_value.location(), old_value.size(), self.atime);

        let pending_len = self.pending_changes.len()
            + self
                .merge
                .as_ref()
                .map(|ms| ms.old_pending_changes.len())
                .unwrap_or_default();

        // XXX looking up again.  But can't pass in both &mut self and &mut PendingChange
        match self.pending_changes.entry(key) {
            btree_map::Entry::Vacant(ve) => {
                // Only in Index, not pending_changes.
                if pending_len < self.pending_changes_cap {
                    // Perserve the original atime (from the Index) in case we "replace" this
                    // block and need to reset the histogram for the original block (i.e. when we
                    // find the old block during the merge, we can decrement the atime histogram)
                    super_trace!(
                        "adding PendingChanges::UpdateAtime({:?}) for {:?}",
                        new_value,
                        key
                    );
                    with_alloctag(Self::PENDING_CHANGES_TAG, || {
                        ve.insert(PendingChange::UpdateAtime(UpdateAtime(
                            new_value, old_atime,
                        )))
                    });
                    self.atime_histogram.remove(old_value);
                    self.atime_histogram.insert(new_value);
                } else {
                    trace!(
                        "pending changes limit reached (now {}), refusing UpdateAtime for {:?}",
                        pending_len,
                        key,
                    );
                }
            }
            btree_map::Entry::Occupied(mut oe) => match oe.get_mut() {
                PendingChange::Insert(value_ref)
                | PendingChange::UpdateAtime(UpdateAtime(value_ref, _)) => {
                    *value_ref = new_value;
                    self.atime_histogram.remove(old_value);
                    self.atime_histogram.insert(new_value);
                }
            },
        }

        self.peek(locked_key, valid_value)
    }

    /// Insert this block to the cache, if space and performance parameters allow.  It may be a
    /// recent cache miss, or a recently-written block.  Returns a Future to be executed after
    /// the state lock has been dropped.
    fn insert(
        &mut self,
        locked_key: LockedKey,
        bytes: AlignedBytes,
    ) -> impl Future<Output = Result<(), InsertError>> {
        let pending_len = self.pending_changes.len()
            + self
                .merge
                .as_ref()
                .map(|ms| ms.old_pending_changes.len())
                .unwrap_or_default();
        if pending_len >= self.pending_changes_cap {
            trace!(
                "pending changes limit reached (now {}), refusing insert of {:?}",
                pending_len,
                locked_key.key()
            );
            return future::ready(Err(InsertError::PendingChanges)).left_future();
        }

        let buf_size = bytes.len();
        let location = match self.allocate_block(u32::try_from(buf_size).unwrap()) {
            Some(location) => location,
            None => return future::ready(Err(InsertError::Allocation)).left_future(),
        };

        // XXX if this is past the last block of the main index, we can write it
        // there (and location_dirty:false) instead of logging it

        let key = locked_key.key();
        let value = IndexValue::new(Some(location), u32::try_from(buf_size).unwrap(), self.atime);

        if let Some(pc) = with_alloctag(Self::PENDING_CHANGES_TAG, || {
            self.pending_changes
                .insert(key, PendingChange::Insert(value))
        }) {
            debug!("{key:?}: inserting {value:?} over existing entry {pc:?}, should be heal");
            let old_value = match pc {
                PendingChange::Insert(old_value) => {
                    // Free the old extent for the previous insert. This is safe because the
                    // previous insert happened after any merge (and rebalance) may have started
                    // so is not going to be impacted by slab eviction.
                    self.block_allocator.free(old_value.extent().unwrap());
                    old_value
                }
                // Undo the atime histogram change made with this UpdateAtime entry
                PendingChange::UpdateAtime(UpdateAtime(old_value, index_atime)) => {
                    self.atime_histogram.insert(IndexValue::new(
                        old_value.location(),
                        old_value.size(),
                        index_atime,
                    ));
                    old_value
                }
            };
            assert_ne!(value, old_value);
            self.atime_histogram.remove(old_value);
        }
        self.atime_histogram.insert(value);

        super_trace!("adding Insert to operation_log {:?} {:?}", key, value);
        self.operation_log
            .push(OperationLogEntry::Insert(key, value));

        let write_permit = self.outstanding_writes.acquire();

        let block_access = self.block_access.clone();
        async move {
            block_access
                .write_raw(location, bytes, DiskIoType::WriteDataForInsert)
                .await;

            // We need to move the write_permit and locked_key guards into this closure so that
            // the locks are held until the write completes. `drop()` serves to do this and
            // indicate that they are moved here just to be dropped at the right time.

            // It's now OK for a checkpoint to complete, persisting the index
            // entry that references this block.
            drop(write_permit);

            // It's now OK to read from this location.
            drop(locked_key);

            Ok(())
        }
        .right_future()
    }

    /// returns offset, or None if there's no space
    fn allocate_block(&mut self, size: u32) -> Option<DiskLocation> {
        self.block_allocator.allocate(size).map(|extent| {
            self.block_access.verify_aligned(extent.location.offset());
            extent.location
        })
    }

    /// Flush out the current set of pending index changes. This is a recovery point in case of
    /// a system crash between index rewrites.
    async fn flush_checkpoint(
        &mut self,
        old_index: IndexRunPhys,
        new_index: Option<IndexRunPhys>,
        completed_merge: bool,
        pool_guids: PoolGuidMappingPhys,
    ) {
        debug!(
            "flushing checkpoint {:?}",
            self.primary.checkpoint_id.next()
        );

        let begin_checkpoint = Instant::now();

        debug!(
            "{:?} pending changes at checkpoint",
            self.pending_changes.len()
        );

        let begin = Instant::now();
        let operation_log_len = self.operation_log.pending_len();
        let bytes = self.operation_log.num_bytes();
        let operation_log_phys = self.operation_log.flush().await;
        let operation_log_bytes = self.operation_log.num_bytes() - bytes;
        debug!(
            "operation log: flushed {} entries to {} in {}ms",
            operation_log_len,
            nice_p2size(operation_log_bytes),
            begin.elapsed().as_millis()
        );

        let merge_progress_phys = match (self.merge.as_ref(), new_index) {
            (None, None) => None,
            (Some(ms), Some(new_index_phys)) => Some(MergeProgressPhys {
                rebalance_log: ms
                    .rebalance
                    .as_ref()
                    .map(|rebalance| rebalance.log_phys.clone()),
                operation_log: ms.old_operation_log_phys.clone(),
                new_index: new_index_phys,
            }),
            _ => panic!("merges must match"),
        };

        let old_index_size = old_index.log_capacity_bytes(self.slab_allocator.access());
        let checkpoint = CheckpointPhys {
            id: self.primary.checkpoint_id.next(),
            pool_guids,
            slab_allocator: self.slab_allocator.get_phys(),
            old_index,
            operation_log: operation_log_phys,
            last_atime: self.atime,
            block_allocator: self.block_allocator.flush(completed_merge).await,
            size_histogram: self.size_histogram.clone(),
            merge_progress: merge_progress_phys,
        };

        let checkpoint_extents = checkpoint
            .write(&self.block_access, &self.slab_allocator)
            .await;

        for extent in mem::replace(&mut self.primary.checkpoint, checkpoint_extents) {
            self.slab_allocator
                .free(self.slab_allocator.extent_to_slab_id(extent));
        }
        self.primary.checkpoint_id = self.primary.checkpoint_id.next();
        self.primary.feature_flags = SUPPORTED_FEATURES.keys().cloned().collect();
        // We need to write all the disks' superblocks in case new disks have been added.
        self.primary
            .write_all(self.primary_disk, self.guid, &self.block_access)
            .await;

        self.slab_allocator.set_reservation(
            Percent::new(150.0).apply(
                old_index_size + (self.pending_changes_cap * size_of::<IndexEntry>()) as u64,
            ),
        );
        self.slab_allocator.release_frees();

        info!(
            "completed {:?} in {}ms; flushed {} operations ({}) to log",
            self.primary.checkpoint_id,
            begin_checkpoint.elapsed().as_millis(),
            operation_log_len,
            nice_p2size(operation_log_bytes),
        );
    }

    fn spawn_merge_task(
        &self,
        merge: Arc<MergeState>,
        old_index: Arc<tokio::sync::RwLock<IndexRun>>,
        mut next_index: IndexRun,
    ) -> mpsc::Receiver<MergeMessage> {
        // The checkpoint task will be constantly reading from the channel, so we don't really need
        // much of a buffer here. We use 100 because we might accumulate some messages while
        // actually flushing out the checkpoint.
        let (index_tx, checkpoint_rx) = mpsc::channel(100);
        let (merge_tx, index_rx) = mpsc::channel(100);

        let block_access = self.block_access.clone();
        let spawn_merge = merge.clone();
        let start_key = next_index.last_key();

        measure!("MergeState::merge_task()").spawn(async move {
            spawn_merge
                .merge_task(merge_tx, old_index, start_key, &block_access)
                .await;
        });

        measure!("MergeState::next_index_task()").spawn(async move {
            merge
                .next_index_task(index_rx, index_tx.clone(), &mut next_index)
                .await;

            // We drop this before sending the Complete message, so that rotate_index() can unwrap
            // the Arc.
            drop(merge);
            // XXX - wait for the merge_task to complete as well?

            // send the now complete next_index as the final message
            index_tx
                .send(MergeMessage::Complete(next_index))
                .await
                .unwrap_or_else(|e| panic!("couldn't send: {}", e));

            trace!("sent final checkpoint message");
        });

        checkpoint_rx
    }

    /// Restart a merge task from the saved checkpoint state
    async fn resume_merge_task(
        &mut self,
        old_index: Arc<tokio::sync::RwLock<IndexRun>>,
        old_pending_changes: PendingChanges,
        progress: MergeProgressPhys,
    ) -> (mpsc::Receiver<MergeMessage>, IndexRunPhys) {
        let next_index = IndexRun::open(
            self.block_access.clone(),
            self.slab_allocator.clone(),
            progress.new_index.clone(),
        )
        .await;
        info!(
            "restarting merge at {:?} with eviction atime {:?}",
            next_index.last_key(),
            next_index.first_ghost_atime(),
        );

        let rebalance = match progress.rebalance_log {
            None => None,
            Some(log_phys) => {
                let map: BTreeMap<Extent, Option<DiskLocation>> = log_phys
                    .iter(self.block_access.clone(), self.slab_allocator.access())
                    .map(|entry| (entry.old, entry.new))
                    .collect()
                    .await;

                Some(RebalanceState { log_phys, map })
            }
        };

        let merge = Arc::new(MergeState {
            old_operation_log_phys: progress.operation_log,
            ghost_cutoff: next_index.first_ghost_atime(),
            eviction_cutoff: next_index.first_live_atime(),
            old_pending_changes,
            rebalance,
            stats: self.stats.clone(),
        });
        self.merge = Some(merge.clone());

        (
            self.spawn_merge_task(merge, old_index, next_index),
            progress.new_index,
        )
    }

    fn space_to_evict(&self) -> u64 {
        let allocatable_from_slabs = self.slab_allocator.allocatable_bytes();
        let allocatable_from_blocks = self.block_allocator.available();
        let target_allocatable = TARGET_FREE_BLOCKS_PCT.apply(self.slab_allocator.capacity());
        let reduction = target_allocatable
            .saturating_sub(allocatable_from_slabs)
            .saturating_sub(allocatable_from_blocks);

        info!(
            "want to evict {} of allocated blocks ({} allocatable slabs; {} allocatable blocks; {} target; {} freeing; {} histogram)",
            nice_p2size(reduction),
            nice_p2size(allocatable_from_slabs),
            nice_p2size(allocatable_from_blocks),
            nice_p2size(target_allocatable),
            nice_p2size(self.block_allocator.freeing()),
            nice_p2size(self.atime_histogram.sum_live()),
        );
        reduction
    }

    fn need_merge(&self) -> bool {
        let mut need_merge = false;

        {
            let used = self.pending_changes.len();
            let max = self.pending_changes_trigger;
            if used > max {
                debug!("starting merge due to pending changes trigger {used} > {max}");
                need_merge = true;
            }
        }

        {
            let reduction = self.space_to_evict();
            if reduction > EVICTION_MIN_BATCH_PCT.apply(self.slab_allocator.capacity()) {
                debug!(
                    "starting merge due to eviction of {}",
                    nice_p2size(reduction)
                );
                need_merge = true;
            }
        }

        {
            let slabs = self.slab_allocator.num_slabs_to_evacuate();
            if slabs > EVACUATION_MIN_BATCH_PCT.apply(self.slab_allocator.num_slabs()) {
                debug!("starting merge due to rebalance of {slabs} slabs");
                need_merge = true;
            }
        }

        need_merge
    }

    /// Start a new merge task if there are enough pending changes
    async fn try_start_merge_task(
        &mut self,
        old_index: Arc<tokio::sync::RwLock<IndexRun>>,
    ) -> Option<(mpsc::Receiver<MergeMessage>, IndexRunPhys)> {
        if !self.need_merge() {
            return None;
        }

        let reduction = self.space_to_evict();
        let eviction_atime = self.atime_histogram.atime_for_eviction_target(reduction);

        let ghost_size = self.atime_histogram.sum_ghost() + reduction;
        let ghost_target = GHOST_CACHE_SIZE_PCT.0.apply(self.slab_allocator.capacity());
        let ghost_reduction = ghost_size.checked_sub(ghost_target).unwrap_or_default();
        let ghost_atime = self.atime_histogram.atime_for_ghost_target(ghost_reduction);
        debug!(
            "ghost history size: {} (including {} transfering from live), target size: {}, removing {}",
            nice_p2size(ghost_size),
            nice_p2size(reduction),
            nice_p2size(ghost_target),
            nice_p2size(ghost_reduction),
        );

        let old_operation_log_phys = self.operation_log.flush().await;

        // Create an empty operation log that is consistent with the empty pending state.
        // Note that we don't want to just clear the existing operation log, since we are
        // still preserving that in the merging state.
        self.operation_log = BlockBasedLog::open(
            self.block_access.clone(),
            self.slab_allocator.clone(),
            Default::default(),
        );
        let next_index_phys = IndexRunPhys::new(ghost_atime, eviction_atime);
        let next_index = IndexRun::open(
            self.block_access.clone(),
            self.slab_allocator.clone(),
            next_index_phys.clone(),
        )
        .await;

        let rebalance = match self.block_allocator.rebalance_init() {
            None => None,
            Some(map) => {
                let mut log = BlockBasedLog::<RebalanceLogEntry>::open(
                    self.block_access.clone(),
                    self.slab_allocator.clone(),
                    Default::default(),
                );

                for (&old, &new) in map.iter() {
                    log.push(RebalanceLogEntry { old, new });
                }

                let log_phys = log.flush().await;

                // We need to ensure that rebalance() won't copy from blocks that we're still in
                // the middle of writing.  To solve a similar problem, we use the LockedKey to
                // ensure that the reads from lookup() see new writes from insert().  We can't
                // use that method here because we don't yet know what keys correspond to the map
                // entries.  Here we simpy wait for all outstanding writes, which is not ideal to
                // be doing with the state lock held, but it works.
                let begin = Instant::now();
                self.outstanding_writes.rotate().await;
                debug!(
                    "rebalance waited for outstanding_writes in {}ms",
                    begin.elapsed().as_millis()
                );

                Some(RebalanceState { map, log_phys })
            }
        };

        // Set up state with current pending changes and operation log in the new merging state.
        // Note that we are "taking" the current set of pending changes for the merge
        // and leaving an empty log behind to accumulate new changes.
        let merge = Arc::new(MergeState {
            ghost_cutoff: ghost_atime,
            eviction_cutoff: eviction_atime,
            old_pending_changes: std::mem::take(&mut self.pending_changes),
            old_operation_log_phys,
            rebalance,
            stats: self.stats.clone(),
        });
        self.merge = Some(merge.clone());

        Some((
            self.spawn_merge_task(merge, old_index, next_index),
            next_index_phys,
        ))
    }

    /// Switch to the new index returned from the merge task and clear the merging state.
    /// Called with the old index write-locked.
    async fn rotate_index(&mut self, old_index: &mut IndexRun, new_index: IndexRun) {
        let mut merge = Arc::try_unwrap(self.merge.take().unwrap())
            .expect("unable to unwrap merge state during index rotation");

        // Free up the extents that have been allocated for the merge pending state
        merge.old_operation_log_phys.clear(&self.slab_allocator);

        if let Some(rebalance) = &merge.rebalance {
            let begin = Instant::now();

            let mut evicted_keys = Vec::new();
            for (key, pc) in self.pending_changes.iter_mut() {
                match pc {
                    PendingChange::Insert(value) => {
                        // Inserts in the "pending changes" list will never be moved as part of a
                        // rebalance.
                        let extent = value.extent().unwrap();
                        assert_eq!(rebalance.remap(extent).unwrap(), extent.location);
                    }
                    PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                        // If a lookup occurs on a block that is being moved as part of
                        // rebalancing, the lookup will return the "old" location of the block
                        // (which is valid while we are merging) and will be stored in an
                        // UpdateAtime record in pending_changes. Now that the merge is complete,
                        // we need to either: 1) remap these old locations to their "new"
                        // rebalanced locations, or 2) remove the UpdateAtime due to rebalancing
                        // having had to evict the entry from the cache (i.e. due to an
                        // allocation failure when attempting to allocate the new disk location).
                        match rebalance.remap(value.extent().unwrap()) {
                            Some(location) => {
                                value.set_location(Some(location));
                            }
                            None => evicted_keys.push(*key),
                        }
                    }
                }
            }

            for key in evicted_keys.iter() {
                self.pending_changes.remove(key);
            }

            debug!(
                "took {}ms to remap pending changes with {} entries",
                begin.elapsed().as_millis(),
                self.pending_changes.len()
            );
        }

        // Move the "start" of the zettacache state histogram to reflect the new index
        self.atime_histogram.reset_first(merge.ghost_cutoff);
        trace!(
            "reset incore histogram start to {:?}",
            self.atime_histogram.first_ghost()
        );
        self.atime_histogram.reset_first_live(merge.eviction_cutoff);

        if let Some(mut rebalance) = merge.rebalance {
            // Free up the extents that have been allocated for the cache rebalancing
            rebalance.log_phys.clear(&self.slab_allocator);
        }

        // Free up the space used by the old index and rotate in the new index.
        // The entire old index should have been obsoleted by .trim().
        assert!(old_index.atime_histogram().is_empty());
        old_index.clear();
        *old_index = new_index;

        self.merge = None;
    }

    fn clear_hit_data(&mut self) {
        let cache_capacity = self.block_access.total_capacity();
        self.size_histogram = SizeHistogramPhys::new(
            cache_capacity + GHOST_CACHE_SIZE_PCT.0.apply(cache_capacity),
            cache_capacity,
            cache_capacity - self.slab_allocator.capacity(),
            *QUANTILES_IN_SIZE_HISTOGRAM,
        )
    }

    fn update_stats(&self) {
        self.stats
            .track_instantaneous(SlabCapacity, self.slab_allocator.capacity());
        self.stats
            .track_instantaneous(AvailableBlocksSize, self.block_allocator.available());
        self.stats.track_instantaneous(
            AvailableSlabsSize,
            self.slab_allocator.free_slabs() * self.slab_allocator.slab_size(),
        );
        self.stats.track_instantaneous(
            AvailableSpace,
            self.block_allocator.available() + self.slab_allocator.allocatable_bytes(),
        );
        let old_pending = match &self.merge {
            Some(ms) => ms.old_pending_changes.len() as u64,
            None => 0,
        };
        self.stats.track_instantaneous(
            PendingChanges,
            old_pending + self.pending_changes.len() as u64,
        );
    }
}
