use crate::atime_histogram::{AtimeHistogram, AtimeHistogramPhys};
use crate::base_types::*;
use crate::block_access::*;
use crate::block_allocator::zcachedb_dump_slabs;
use crate::block_allocator::zcachedb_dump_spacemaps;
use crate::block_allocator::BlockAllocator;
use crate::block_allocator::BlockAllocatorPhys;
use crate::block_based_log::*;
use crate::extent_allocator::ExtentAllocator;
use crate::extent_allocator::ExtentAllocatorBuilder;
use crate::extent_allocator::ExtentAllocatorPhys;
use crate::extent_allocator::DEFAULT_EXTENT_SIZE;
use crate::features::check_features;
use crate::features::SUPPORTED_FEATURES;
use crate::index::*;
use crate::size_histogram::SizeHistogramPhys;
use crate::superblock::DiskPhys;
use crate::superblock::PrimaryPhys;
use crate::superblock::SuperblockPhys;
use crate::superblock::SUPERBLOCK_SIZE;
use crate::DumpSlabsOptions;
use crate::DumpStructuresOptions;
use anyhow::Result;
use bytes::Bytes;
use conv::ConvUtil;
use either::Either;
use futures::future;
use futures::stream::*;
use futures::Future;
use lazy_static::lazy_static;
use log::*;
use lru::LruCache;
use more_asserts::*;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::btree_map;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::mem;
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use sysinfo::System;
use sysinfo::SystemExt;
use tokio::sync::mpsc;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::time::{sleep_until, timeout_at};
use util::get_tunable;
use util::lock_non_send;
use util::maybe_die_with;
use util::nice_p2size;
use util::super_trace;
use util::with_alloctag;
use util::with_alloctag_hf;
use util::writeln_stderr;
use util::writeln_stdout;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::*;
use util::AlignedBytes;
use util::From64;
use util::LockSet;
use util::LockedItem;
use uuid::Uuid;

lazy_static! {
    static ref DEFAULT_CHECKPOINT_SIZE_PCT: f64 = get_tunable("default_checkpoint_size_pct", 0.1);
    static ref DEFAULT_METADATA_SIZE_PCT: f64 = get_tunable("default_metadata_size_pct", 15.0); // Can lower this to test forced eviction.
    static ref PENDING_CHANGES_MEM_PCT: f64 = get_tunable("pending_changes_mem_pct", 2.0);
    static ref CHECKPOINT_INTERVAL: Duration = Duration::from_secs(get_tunable("checkpoint_interval_secs", 60));
    static ref MERGE_PROGRESS_MESSAGE_INTERVAL: Duration = Duration::from_millis(get_tunable("merge_progress_message_interval_ms", 1000));
    static ref MERGE_PROGRESS_CHUNK: usize = get_tunable("merge_progress_chunk", 1_000_000);
    static ref MERGE_PROGRESS_CHECK_COUNT: u32 = get_tunable("merge_progress_check_count", 100);
    static ref TARGET_CACHE_SIZE_PCT: u64 = get_tunable("target_cache_size_pct", 80);
    static ref HIGH_WATER_CACHE_SIZE_PCT: u64 = get_tunable("high_water_cache_size_pct", 82);
    static ref GHOST_CACHE_SIZE_PCT: u64 = get_tunable("ghost_cache_size_pct", 100);
    static ref QUANTILES_IN_SIZE_HISTOGRAM: usize = get_tunable("quantiles_in_size_histogram", 100);
    static ref CACHE_INSERT_DEMAND_BUFFER_BYTES: usize = get_tunable("cache_insert_demand_buffer_bytes", 256 * 1024 * 1024);
    static ref CACHE_INSERT_SPECULATIVE_BUFFER_BYTES: usize = get_tunable("cache_insert_speculative_buffer_bytes", 256 * 1024 * 1024);
    static ref CACHE_WAIT_INSERT: bool = get_tunable("cache_wait_insert", false);
    static ref INDEX_CACHE_ENTRIES_MEM_PCT: usize = get_tunable("index_cache_entries_mem_pct", 10);
    static ref DISK_EXPAND_MIN_PCT: f64 = get_tunable("disk_expand_min_pct", 10.0);

    // A limit of 8 should be enough to get to the 16,000 IOPS limit of medium-size instances/disks on gp3; because gp3 has ~1ms latency for each operation,
    // and each closure this limit applies to, performs 2 operations (one read, and one write). Additionally, this is half the limit of outstanding writes.
    static ref CACHE_REBALANCE_CONCURRENCY_LIMIT: usize = get_tunable("cache_rebalance_concurrency_limit", 8);

    // If non-zero, the lookup() function will fail randomly every specified number of requests
    static ref LOOKUP_FAIL_RANDOM: u32 = get_tunable("lookup_fail_random", 0);
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MergeProgressPhys {
    rebalance_log: Option<BlockBasedLogPhys<RebalanceLogEntry>>,
    operation_log: BlockBasedLogPhys<OperationLogEntry>,
    new_index: IndexRunPhys,
}

#[derive(Serialize, Deserialize, Debug)]
struct ZettaCheckpointPhys {
    generation: CheckpointId,
    pool_guids: Vec<PoolGuid>,
    extent_allocator: ExtentAllocatorPhys,
    block_allocator: BlockAllocatorPhys,
    last_atime: Atime,
    old_index: IndexRunPhys,
    operation_log: BlockBasedLogPhys<OperationLogEntry>,
    size_histogram: SizeHistogramPhys,
    merge_progress: Option<MergeProgressPhys>,
}

impl ZettaCheckpointPhys {
    async fn read(block_access: &BlockAccess, extent: Extent) -> ZettaCheckpointPhys {
        let raw = block_access
            .read_raw(extent, DiskIoType::MaintenanceRead)
            .await;
        let (this, _): (Self, usize) = block_access.chunk_from_raw(&raw).unwrap();
        debug!("got {:#?}", this);
        this
    }

    fn claim(&self, builder: &mut ExtentAllocatorBuilder) {
        self.block_allocator.claim(builder);
        self.old_index.claim(builder);
        self.operation_log.claim(builder);
        if let Some(progress) = self.merge_progress.as_ref() {
            progress.operation_log.claim(builder);
            progress.new_index.claim(builder);
            if let Some(rebalance_log) = progress.rebalance_log.as_ref() {
                rebalance_log.claim(builder);
            }
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
pub struct ZettaCache {
    block_access: Arc<BlockAccess>,

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
    write_slots: Arc<Semaphore>,
    cache_runtime_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
struct ChunkSummaryEntry {
    offset: LogOffset,
    first_key: IndexKey,
}
impl OnDisk for ChunkSummaryEntry {}
impl BlockBasedLogEntry for ChunkSummaryEntry {}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
enum OperationLogEntry {
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
    remaps: Vec<IndexEntry>,
    obsoleted: AtimeHistogramPhys, // entries obsoleted from old index, since last MergeProgress
}

#[derive(Debug)]
struct MergeProgress {
    new_index: IndexRunPhys,
    obsoleted: AtimeHistogramPhys,
    index_delta: IndexFlushDelta,
    frees: Vec<Extent>,
    remaps: Vec<IndexEntry>,
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
        remaps: Vec<IndexEntry>,
        obsoleted: AtimeHistogramPhys,
    ) -> Self {
        let timer = Instant::now();
        let free_count = frees.len();
        let remap_count = remaps.len();
        let (new_index, index_delta) = next_index.flush().await;
        let message = MergeProgress {
            new_index,
            index_delta,
            obsoleted,
            frees,
            remaps,
        };
        debug!("sending progress: index with {} entries ({}) last is {:?} flushed in {}ms, and {} frees, and {} remap requests",
            next_index.len(),
            nice_p2size(next_index.num_bytes()),
            next_index.last_key(), timer.elapsed().as_millis(),
            free_count,
            remap_count);
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
                        // This represents the offset of the passed in extent, into the extent that was moved as part
                        // of the rebalance operation. For example, multiple contiguously allocated blocks maybe have
                        // been moved via a single extent. Thus, to remap one of those blocks' to it's new location on
                        // disk, we need this offset (this offset is maintained when the blocks are copied).
                        let offset = extent.location - old.location;

                        return Some(DiskLocation::new(
                            new_location.disk(),
                            new_location.offset() + offset,
                        ));
                    }
                    None => {
                        // This means the extent was part of a rebalance operation, but when attempting to remap
                        // the old location to a new location, the allocation failed. Thus, the old extent does not
                        // have new location, and it will be invalid after the rebalance completes.
                        return None;
                    }
                }
            }
        }

        // If we reach this point, we didn't find an extent in the mapping that contains the passed in extent, which means
        // the passed in extent was not remapped; thus, we simply return the old extent's location.
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
                        message.remaps,
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

    /// This function runs in an async task to merge a set of pending changes with the current on-disk
    /// index in order to produce a new up-to-date on-disk index. It sends periodic "progress updates"
    /// (including block frees) to the checkpoint task.
    async fn merge_task(
        &self,
        tx: mpsc::Sender<IndexMessage>,
        old_index_lock: Arc<tokio::sync::RwLock<IndexRun>>,
        start_key: Option<IndexKey>,
        block_access: &BlockAccess,
    ) {
        // We don't currently support concurrent free()'s while the rebalance is in-progress. Thus, we
        // need to do the rebalance first, prior to moving forward with the merge.
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
            remaps: Vec<IndexEntry>,
            obsoleted: AtimeHistogramPhys,
            timer: Instant,
        }
        impl Progress {
            fn new(tx: mpsc::Sender<IndexMessage>, first_ghost: Atime, first_live: Atime) -> Self {
                Self {
                    tx,
                    last_key: None,
                    entries: Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                    frees: Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                    remaps: Vec::with_capacity(*MERGE_PROGRESS_CHUNK),
                    obsoleted: AtimeHistogramPhys::new(first_ghost, first_live),
                    timer: Instant::now(),
                }
            }

            /// As entries from the old index are processed (possibly added to the new index),
            /// they are now "obsolete" in the old index, so need to be removed from the atime histogram.
            fn obsolete(&mut self, entry: IndexEntry) {
                self.obsoleted.insert(entry.value);
            }

            /// When an old index entry already exists for a newly inserted key, the new entry will
            /// replace the old, so "evict" the old entry: if the entry is a ghost, then there is
            /// nothing to do, otherwise, add the entry to the free list.
            async fn evict(&mut self, entry: IndexEntry) {
                if let Some(extent) = entry.value.extent() {
                    self.frees.push(extent);
                    if self.entries.len() >= *MERGE_PROGRESS_CHUNK
                        || self.frees.len() >= *MERGE_PROGRESS_CHUNK
                        || self.remaps.len() >= *MERGE_PROGRESS_CHUNK
                    {
                        self.report().await;
                    }
                }
            }

            /// The provided index entry is either:
            /// 1. Added to the list of entries to be part of the new index, or
            /// 2. Added to the list of entries to be evicted from the cache, or
            /// 3. Dropped because it is an already evicted entry that is no longer being tracked.
            async fn ingest(&mut self, state: &MergeState, mut entry: IndexEntry) {
                if let Some(extent) = entry.value.extent() {
                    if let Some(rebalance) = &state.rebalance {
                        let remapped_location = rebalance.remap(extent);
                        if entry.value.location() != remapped_location {
                            // The data for this entry has been moved due to a cache rebalance operation.
                            // Update the entry using the new location for the data. Note: if rebalance
                            // was unable to move the data (evicting the entry instead) the new location
                            // will be None.
                            entry.value.set_location(remapped_location);
                            self.remaps.push(entry);
                        }
                    }
                }
                if entry.value.atime() >= state.eviction_cutoff {
                    // If this entry was evicted during rebalance, don't put it in the new index
                    if entry.value.location().is_some() {
                        self.entries.push(entry);
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
                    || self.remaps.len() >= *MERGE_PROGRESS_CHUNK
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
                            remaps: mem::replace(
                                &mut self.remaps,
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

        let mut index_skips = 0;
        while let Some(chunk) = index_stream.next().await {
            for &entry in chunk.entries() {
                // If the next index is already "started", advance the old index to the start point
                // XXX - would be nice to simply *start* from the start_key, rather than iterate up to it
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
                            progress.evict(entry).await;
                            progress
                                .ingest(
                                    self,
                                    IndexEntry {
                                        key: pc_key,
                                        value: pc_value,
                                    },
                                )
                                .await;
                            // this pending change is consumed
                            pending_changes_iter.next();
                        } else {
                            assert_gt!(pc_key, entry.key);
                            progress.ingest(self, entry).await;
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
                                )
                                .await;

                            // this pending change is consumed
                            pending_changes_iter.next();
                        } else {
                            // We shouldn't have skipped any, because there has to be a corresponding Index entry
                            assert_gt!(pc_key, entry.key);
                            progress.ingest(self, entry).await;
                        }
                    }
                    None => {
                        // no more pending changes
                        progress.ingest(self, entry).await;
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
                )
                .await;
            // Consume pending change.  We don't do that in the `while let`
            // because we want to leave any unmatched items in the iterator so
            // that we can print them out when failing below.
            pending_changes_iter.next();
        }
        // Other pending changes refer to existing index entries and therefore should have been processed above
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
                *CACHE_REBALANCE_CONCURRENCY_LIMIT,
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
    pool_guids: Vec<PoolGuid>,
    block_allocator: BlockAllocator,
    pending_changes: PendingChanges,
    pending_changes_trigger: usize,
    pending_changes_cap: usize,
    // Keep state associated with any on-going merge here
    merge: Option<Arc<MergeState>>,
    index_cache: LruCache<IndexKey, IndexValue>,
    // XXX Given that we have to lock the entire State to do anything, we might
    // get away with this being a Rc?  And the ExtentAllocator doesn't really
    // need the lock inside it.  But hopefully we split up the big State lock
    // and then this is useful.  Same goes for block_access.
    extent_allocator: Arc<ExtentAllocator>,
    atime_histogram: AtimeHistogram, // includes pending_changes, including AtimeUpdate which is not logged
    size_histogram: SizeHistogramPhys,
    // XXX move this to its own file/struct with methods to load, etc?
    operation_log: BlockBasedLog<OperationLogEntry>,
    // This is needed to ensure that reads complete before we complete the next
    // checkpoint, so that we don't overwrite their locations on disk (if the
    // block is evicted and freed from the cache).
    outstanding_reads: Arc<tokio::sync::RwLock<()>>,
    // This is needed to ensure that writes complete before we complete the next
    // checkpoint, so that they are persisted to disk.
    outstanding_writes: Arc<tokio::sync::RwLock<()>>,

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
    Present((AlignedBytes, LockedKey)),
    Absent(LockedKey),
}

#[derive(Clone, Copy, Debug)]
pub enum InsertSource {
    Heal,
    Read,
    SpeculativeRead,
    Write,
}

#[derive(Clone, Copy, Debug)]
pub enum LookupSource {
    Write,
    Read,
    Evict,
}

pub enum LookupOperation {
    Lookup,
    Evict,
}

fn checkpoint_size(block_access: &BlockAccess) -> u64 {
    block_access.round_up_to_sector(
        (*DEFAULT_CHECKPOINT_SIZE_PCT / 100.0 * block_access.total_capacity() as f64)
            .approx_as::<u64>()
            .unwrap(),
    )
}

impl ZettaCache {
    /// Returns (checkpoint, metadata, data)
    fn divide_new_capacity(
        new_capacity: Vec<Extent>,
        block_access: &BlockAccess,
    ) -> (Extent, Vec<Extent>, Vec<Extent>) {
        // The checkpoint is stored on the largest provided disk (when adding disks,
        // only the new disks are candidates). Its size is a percent of the whole
        // cache.
        let checkpoint_capacity = new_capacity
            .iter()
            .max_by_key(|extent| extent.size)
            .unwrap()
            .range(0, checkpoint_size(block_access));

        // metadata is stored on each disk, its size a percent of that disk, following the checkpoint (if any)
        let metadata_capacity = new_capacity
            .iter()
            .map(|&extent| {
                match extent.after(&checkpoint_capacity) {
                    Some(after) => after,
                    None => extent,
                }
                .range(
                    0,
                    block_access.round_up_to_sector(
                        (*DEFAULT_METADATA_SIZE_PCT / 100.0 * extent.size as f64)
                            .approx_as::<u64>()
                            .unwrap(),
                    ),
                )
            })
            .collect::<Vec<_>>();

        // remaining capacity is for data
        let data_capacity = new_capacity
            .iter()
            .zip(metadata_capacity.iter())
            .map(|(new, metadata)| new.after(metadata).unwrap())
            .collect();

        (checkpoint_capacity, metadata_capacity, data_capacity)
    }

    pub async fn create(block_access: &BlockAccess) {
        let guid: u64 = rand::random();

        let total_capacity = block_access.total_capacity();
        info!("creating cache from {} disks", block_access.disks().count());

        let new_capacity = block_access
            .disks()
            .map(|disk| Extent::new(disk, SUPERBLOCK_SIZE, block_access.disk_size(disk)))
            .collect();
        let (checkpoint_capacity, metadata_capacity, data_capacity) =
            Self::divide_new_capacity(new_capacity, block_access);

        let checkpoint = ZettaCheckpointPhys {
            generation: CheckpointId(0),
            pool_guids: Vec::new(),
            block_allocator: BlockAllocatorPhys::new(data_capacity),
            extent_allocator: ExtentAllocatorPhys::new(metadata_capacity),
            old_index: IndexRunPhys::new(Atime(0), Atime(0)),
            operation_log: Default::default(),
            last_atime: Atime(0),
            size_histogram: SizeHistogramPhys::new(
                total_capacity + (total_capacity / 100 * *GHOST_CACHE_SIZE_PCT),
                total_capacity,
                (total_capacity as f64 * *DEFAULT_METADATA_SIZE_PCT / 100.0)
                    .approx_as::<u64>()
                    .unwrap(),
                *QUANTILES_IN_SIZE_HISTOGRAM,
            ),

            merge_progress: None,
        };
        let raw = block_access.chunk_to_raw(EncodeType::Json, &checkpoint);
        let checkpoint_extent = checkpoint_capacity.range(0, raw.len() as u64);

        block_access
            .write_raw(
                checkpoint_extent.location,
                raw,
                DiskIoType::MaintenanceWrite,
            )
            .await;
        PrimaryPhys {
            checkpoint_id: CheckpointId(0),
            checkpoint_capacity,
            old_checkpoint_capacity: Vec::new(),
            checkpoint: checkpoint_extent,
            feature_flags: SUPPORTED_FEATURES.keys().cloned().collect(),
            disks: block_access
                .disks()
                .map(|disk| {
                    (
                        disk,
                        DiskPhys {
                            size: block_access.disk_size(disk),
                        },
                    )
                })
                .collect(),
        }
        .write_all(DiskId::new(0), guid, block_access)
        .await;
    }

    fn index_cache_estimate_capacity(system_memory: usize) -> usize {
        // Calculate the maximum size for the index cache as a percentage of system memory
        let target_index_cache_bytes = (*INDEX_CACHE_ENTRIES_MEM_PCT * system_memory) / 100;

        // Looking at the source of LruCache at the time of this writing we see that LruEntry<K,V>
        // is composed of the following elements: K, V, and 2 pointers. Thus, we use the following
        // formula to approximate the size of each entry in the index cache which empirically seem
        // to be fairly accurate:
        let index_cache_entry_size =
            mem::size_of::<IndexKey>() + mem::size_of::<IndexValue>() + 2 * mem::size_of::<usize>();

        // Even when the cache is empty LruCache pre-allocates buckets inducing an overhead that is
        // separate from the actual per entry overhead yet tied to the number of entries that it can
        // hold.  The cache overhead consists of a tiny constant overhead for some of its metadata
        // tracking (e.g. capacity, hasher fields, etc..) and per-entry overhead. At the time of this
        // writing the LruCache uses a KeyRef<K> (8 bytes) for the key, and a Box<LruEntry> (8 bytes)
        // as the value. Additionally assuming that HashBrown is used as the underlying HashMap we
        // expect 8 + 1 bytes of overhead per entry. That would imply that the overhead be close to
        // 3 * sizeof(usize) per entry but empirically we've found that it is closer to 5 * sizeof(usize).
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

    pub async fn open(paths: Vec<&str>) -> Result<ZettaCache> {
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
            panic!("{}", feature_error)
        };

        let (mut primary, primary_disk, guid, extra_disks) =
            PrimaryPhys::read(&block_access).await.unwrap();

        // XXX proper error handling
        assert!(primary.checkpoint_capacity.contains(&primary.checkpoint));
        let mut checkpoint = ZettaCheckpointPhys::read(&block_access, primary.checkpoint).await;
        assert_eq!(checkpoint.generation, primary.checkpoint_id);

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
                .collect();
            let (checkpoint_capacity, metadata_capacity, data_capacity) =
                Self::divide_new_capacity(new_capacity, &block_access);

            primary.disks.extend(extra_disks.iter().map(|&disk| {
                (
                    disk,
                    DiskPhys {
                        size: block_access.disk_size(disk),
                    },
                )
            }));

            primary
                .old_checkpoint_capacity
                .push(primary.checkpoint_capacity);
            primary.checkpoint_capacity = checkpoint_capacity;
            checkpoint.extent_allocator.extend(metadata_capacity);
            checkpoint.block_allocator.extend(data_capacity);
            size_changed = true;
        }

        let expanded_capacity = primary
            .disks
            .iter()
            .filter_map(|(&disk, phys)| {
                let new_size = block_access.disk_size(disk);
                if new_size > phys.size {
                    let added_bytes = new_size - phys.size;
                    // Added space must be at least large enough for the checkpoint and one slab.
                    if added_bytes
                        > checkpoint_size(&block_access) + checkpoint.block_allocator.slab_size()
                        && added_bytes as f64 > phys.size as f64 * *DISK_EXPAND_MIN_PCT / 100.0
                    {
                        Some(Extent::new(disk, phys.size, added_bytes))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if !expanded_capacity.is_empty() {
            info!("expanding existing disks: {:?}", expanded_capacity);
            let (checkpoint_capacity, metadata_capacity, data_capacity) =
                Self::divide_new_capacity(expanded_capacity, &block_access);
            primary
                .old_checkpoint_capacity
                .push(primary.checkpoint_capacity);
            primary.checkpoint_capacity = checkpoint_capacity;
            checkpoint.extent_allocator.extend(metadata_capacity);
            checkpoint.block_allocator.extend(data_capacity);

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

        let mut builder = ExtentAllocatorBuilder::new(&checkpoint.extent_allocator);
        checkpoint.claim(&mut builder);
        let extent_allocator = Arc::new(ExtentAllocator::open(builder));

        let operation_log = BlockBasedLog::open(
            block_access.clone(),
            extent_allocator.clone(),
            checkpoint.operation_log,
        );

        let old_index = IndexRun::open(
            block_access.clone(),
            extent_allocator.clone(),
            checkpoint.old_index,
        )
        .await;

        // Note, the old_index histogram covers only the part that doesn't overlap with the new_index.
        let mut atime_histogram_phys = old_index.atime_histogram().clone();
        if let Some(merge_progress) = &checkpoint.merge_progress {
            assert_eq!(old_index.trim_key(), merge_progress.new_index.last_key());
            atime_histogram_phys += merge_progress.new_index.atime_histogram();
        }

        let (old_pending_changes, new_index) = match &checkpoint.merge_progress {
            Some(merge_progress) => {
                let old_operation_log = BlockBasedLog::open(
                    block_access.clone(),
                    extent_allocator.clone(),
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
        // Note that during a merge, this space must also accomodate the space used by old_pending_changes
        let pending_changes_max_bytes = (*PENDING_CHANGES_MEM_PCT * system_memory as f64)
            .approx_as::<usize>()
            .unwrap()
            / 100;
        // The BTreeMap type has about a 35% overhead, so we have a 65% usable capacity for data entries
        let pending_changes_entries_bytes = pending_changes_max_bytes * 65 / 100;
        // Each entry in the BTreeMap is comprised of a key (IndexKey) and a value (PendingChange)
        let pending_changes_entry_size =
            mem::size_of::<IndexKey>() + mem::size_of::<PendingChange>();
        // Limit the number of pending change entries to not exceed the amount of memory being made available
        let pending_changes_cap = pending_changes_entries_bytes / pending_changes_entry_size;
        // In order to stay inside this desired cap, we need to be triggering a new merge before we are more
        // than half way to the cap. Trigger at about 1/3 to provide some slop space.
        let pending_changes_trigger = pending_changes_cap / 3;
        info!(
            "pending changes max length set to {} entries [{}% of {} = {} and entry size {}]",
            pending_changes_cap,
            *PENDING_CHANGES_MEM_PCT,
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
                LruCache::new(ZettaCache::index_cache_estimate_capacity(system_memory))
            }),
            atime_histogram: AtimeHistogram::new(atime_histogram_phys),
            size_histogram: checkpoint.size_histogram,
            operation_log,
            primary,
            primary_disk,
            guid,
            pool_guids: checkpoint.pool_guids,
            outstanding_reads: Default::default(),
            outstanding_writes: Default::default(),
            atime: checkpoint.last_atime,
            block_allocator: BlockAllocator::open(
                block_access.clone(),
                extent_allocator.clone(),
                checkpoint.block_allocator,
            )
            .await,
            extent_allocator,
            stats: stats.clone(),
        };

        // Now that BlockAllocator is open grab its size stats (these will be updated periodically)
        stats.track_instantaneous(BlockAllocatorSize, state.block_allocator.size());
        stats.track_instantaneous(BlockAllocatorAvailable, state.block_allocator.available());
        stats.track_instantaneous(
            BlockAllocatorFreeSlabsSize,
            state.block_allocator.free_slabs_size(),
        );

        if size_changed {
            // The hit data isn't accurate across cache size changes, so clear
            // it, which also updates the histogram parameters to reflect the
            // new cache size.
            state.clear_hit_data();
        }

        let this = ZettaCache {
            old_index: Arc::new(tokio::sync::RwLock::new(old_index)),
            new_index: Arc::new(tokio::sync::RwLock::new(new_index)),
            state: Arc::new(tokio::sync::Mutex::new(state)),
            outstanding_lookups: LockSet::new(),
            demand_buffer_bytes_available: Arc::new(Semaphore::new(
                *CACHE_INSERT_DEMAND_BUFFER_BYTES,
            )),
            speculative_buffer_bytes_available: Arc::new(Semaphore::new(
                *CACHE_INSERT_SPECULATIVE_BUFFER_BYTES,
            )),
            write_slots: Arc::new(Semaphore::new(
                block_access.disks().count() * *DISK_WRITE_MAX_QUEUE_DEPTH,
            )),
            block_access,
            stats,
            timebase: Instant::now(),
            cache_runtime_id: Uuid::new_v4(),
        };

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
        tokio::spawn(async move {
            my_cache.checkpoint_task(merging).await;
        });

        let state = this.state.clone();
        tokio::spawn(async move {
            // XXX maybe we should bump the atime after a set number of
            // accesses, so each histogram bucket starts with the same count.
            // We could then add an auxiliary structure saying what wall clock
            // time each atime value corresponds to.
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                let mut state = state.lock().await;
                state.atime = state.atime.next();
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
                            // We are replacing an old value, adjust the histogram to reflect the change
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

    /// The checkpoint task is primarily responsible for writing out a persistent checkpoint every 60s.
    /// It is also responsible for kicking off a merge task every time we accumulate enough pending change.
    /// While a merge task is running, this task listens for and processes eviction requests from the merge task.
    /// The active merge task state is also captured in each checkpoint so that it may be resumed from the
    /// checkpoint if necessary. On resume the merge task is restarted during cache open and a channel to
    /// task and the index phys for the current progress are passed in.
    async fn checkpoint_task(
        &self,
        mut merging: Option<(mpsc::Receiver<MergeMessage>, IndexRunPhys)>,
    ) {
        let mut next_tick = tokio::time::Instant::now();
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
                let mut msg_count = 0;
                let mut free_count = 0;
                let mut remap_count = 0;
                // we have a channel to an active merge task, check it for messages
                loop {
                    let result = timeout_at(next_tick, rx.recv()).await;
                    match result {
                        // capture merge progress: the current next index phys and eviction requests
                        Ok(Some(MergeMessage::Progress(merge_progress))) => {
                            msg_count += 1;
                            free_count += merge_progress.frees.len();
                            remap_count += merge_progress.remaps.len();
                            trace!(
                                "merge checkpoint with {} free requests and {} remap requests",
                                merge_progress.frees.len(),
                                merge_progress.remaps.len()
                            );
                            super_trace!("eviction requested for {:?}", merge_progress.frees);
                            super_trace!("remap requested for {:?}", merge_progress.remaps);
                            {
                                let mut state = self.state.lock().await;

                                // free the extent ranges associated with the evicted blocks
                                for extent in merge_progress.frees {
                                    state.block_allocator.free(extent);
                                }

                                // update any remapped (possibly evicted) locations in the index cache
                                for entry in &merge_progress.remaps {
                                    match entry.value.location() {
                                        Some(location) => {
                                            if let Some(value) =
                                                state.index_cache.peek_mut(&entry.key)
                                            {
                                                value.set_location(Some(location));
                                            }
                                        }
                                        None => {
                                            state.index_cache.pop(&entry.key);
                                        }
                                    }
                                }
                            } // drop state lock

                            *new_index_phys = merge_progress.new_index;
                            let mut old_index = self.old_index.write().await;
                            let mut new_index_opt = self.new_index.write().await;
                            match &mut *new_index_opt {
                                Some(new_index) => {
                                    new_index.update(
                                        new_index_phys.clone(),
                                        &merge_progress.index_delta,
                                    );
                                }
                                None => {
                                    *new_index_opt = Some(
                                        ReadOnlyIndexRun::open(
                                            self.block_access.clone(),
                                            new_index_phys.clone(),
                                        )
                                        .await,
                                    );
                                }
                            }
                            if let Some(last_key) = new_index_phys.last_key() {
                                old_index.trim(last_key, &merge_progress.obsoleted);
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
                            break;
                        }
                        Ok(None) => panic!("channel closed before Complete message received"),
                        Err(_) => break, // timed out
                    }
                }
                debug!(
                    "processed {} merge checkpoints with {} evictions and {} remaps in {}ms",
                    msg_count,
                    free_count,
                    remap_count,
                    begin.elapsed().as_millis()
                );
            }

            // flush out a new checkpoint every CHECKPOINT_INTERVAL to capture the current state
            sleep_until(next_tick).await;
            {
                let old_index_phys = self.old_index.write().await.get_phys();
                self.state
                    .lock()
                    .await
                    .flush_checkpoint(
                        old_index_phys,
                        merging.as_mut().map(|(_, phys)| (phys.clone())),
                    )
                    .await;
            }
            next_tick = std::cmp::max(
                tokio::time::Instant::now(),
                next_tick + *CHECKPOINT_INTERVAL,
            );
        }
    }

    pub async fn lookup(
        &self,
        guid: PoolGuid,
        block: BlockId,
        source: LookupSource,
    ) -> LookupResponse {
        let key = IndexKey::new(self.state.lock().await.map_pool_guid(guid), block);
        let locked_key = LockedKey(self.outstanding_lookups.lock(key).await);

        // In debug mode, return failure randomly every specified number of requests
        if *LOOKUP_FAIL_RANDOM != 0 && rand::thread_rng().gen_ratio(1, *LOOKUP_FAIL_RANDOM) {
            return LookupResponse::Absent(locked_key);
        }

        let bytes = self
            .lookup_impl(&locked_key, source, |state, value| {
                if matches!(source, LookupSource::Read) {
                    state.size_histogram.lookup();
                }
                match value {
                    Some(value) => future::Either::Left(state.lookup(&locked_key, value, source)),
                    None => future::Either::Right(future::ready(None)),
                }
            })
            .await;

        let response = match bytes {
            Some(bytes) => {
                self.stats.track_bytes(LookupBytes, bytes.len() as u64);
                super_trace!("cache hit for {:?}", key);
                self.stats.track_count(CacheHit);
                LookupResponse::Present((bytes, locked_key))
            }
            None => LookupResponse::Absent(locked_key),
        };

        match source {
            LookupSource::Write => self.stats.track_count(LookupForWrite),
            LookupSource::Read => self.stats.track_count(LookupForRead),
            LookupSource::Evict => {} // not possible for this code path
        }

        response
    }

    async fn lookup_impl<F, R, Fut>(&self, locked_key: &LockedKey, source: LookupSource, f: F) -> R
    where
        F: FnOnce(&mut ZettaCacheState, Option<ValidIndexValue>) -> Fut,
        Fut: Future<Output = R>,
    {
        let key = locked_key.key();
        // Hold the index lock over the whole operation
        // so that the index can't change after we get the value from it.
        // Lock ordering requires that we lock the index before locking the state.
        let old_index_guard = self.old_index.read().await;
        let new_index_guard = self.new_index.read().await;

        let fut_or_f = {
            // We don't want to hold the state lock while reading from disk so we
            // use lock_non_send() to ensure that we can't hold it across .await.
            let mut state = lock_non_send(&self.state).await;
            match state.pending_changes.get(&key).copied() {
                Some(pc) => {
                    match pc {
                        PendingChange::Insert(value)
                        | PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                            let validated = state.validate(value);
                            // All entries in the pending changes should be valid
                            assert!(validated.is_some());
                            if matches!(source, LookupSource::Read) {
                                self.stats.track_count(IndexHitPendingChanges);
                            }
                            Either::Left(f(&mut state, validated))
                        }
                    }
                }
                None => {
                    if let Some(ms) = &state.merge {
                        if let Some(pc) = ms.old_pending_changes.get(&key).copied() {
                            match pc {
                                PendingChange::Insert(value)
                                | PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                                    state.ghost_hit_check(value, source);
                                    let validated = state.validate(value);
                                    if matches!(source, LookupSource::Read) {
                                        self.stats.track_count(IndexHitPendingChanges);
                                    }
                                    Either::Left(f(&mut state, validated))
                                }
                            }
                        } else {
                            match state.index_cache.get(&key) {
                                Some(&value) => {
                                    state.ghost_hit_check(value, source);
                                    let validated = state.validate(value);
                                    if matches!(source, LookupSource::Read) {
                                        self.stats.track_count(IndexHitIndexCache);
                                    }
                                    Either::Left(f(&mut state, validated))
                                }
                                None => Either::Right(f),
                            }
                        }
                    } else {
                        match state.index_cache.get(&key) {
                            Some(&value) => {
                                state.ghost_hit_check(value, source);
                                let validated = state.validate(value);
                                if matches!(source, LookupSource::Read) {
                                    self.stats.track_count(IndexHitIndexCache);
                                }
                                Either::Left(f(&mut state, validated))
                            }
                            None => Either::Right(f),
                        }
                    }
                }
            }
        };

        let f = match fut_or_f {
            Either::Left(fut) => {
                // Got the index entry from pending state or index cache and
                // already called f().  Now that we've dropped the state lock,
                // run the future that it returned.
                return fut.await;
            }
            Either::Right(f) => f,
        };

        super_trace!(
            "lookup has no pending_change for {:?} and it's absent from the index-cache; checking index",
            key
        );

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
            Either::Left(index) => index.lookup(key).await,
            Either::Right(index) => index.lookup(key).await,
        };
        if matches!(source, LookupSource::Read) {
            if chunk_cache_hit {
                self.stats.track_count(IndexHitChunkCache);
            } else {
                self.stats.track_count(IndexHitDisk);
            }
        }
        let fut = match entry_opt {
            Some(entry) => {
                // Again, we don't want to hold the state lock while reading from disk so
                // we use lock_non_send() to ensure that we can't hold it across .await.
                let mut state = lock_non_send(&self.state).await;
                let value = state.lookup_with_value_from_index(&key, entry.value, source);
                if value.is_none() {
                    super_trace!(
                        "cache miss after reading index for {:?}, invalid entry",
                        key
                    );
                }
                f(&mut state, value)
            }
            None => {
                // key not in index
                super_trace!("cache miss after reading index for {:?}", key);
                let mut state = lock_non_send(&self.state).await;
                f(&mut state, None)
            }
        };
        fut.await
    }

    async fn reserve_buffer_space(
        &self,
        bytes: usize,
        source: InsertSource,
    ) -> Option<OwnedSemaphorePermit> {
        let (buffer, size, stat, wait_insert) = match source {
            InsertSource::Heal | InsertSource::SpeculativeRead | InsertSource::Write => (
                &self.speculative_buffer_bytes_available,
                *CACHE_INSERT_SPECULATIVE_BUFFER_BYTES,
                SpeculativeBufferBytesAvailable,
                false,
            ),
            InsertSource::Read => (
                &self.demand_buffer_bytes_available,
                *CACHE_INSERT_DEMAND_BUFFER_BYTES,
                DemandBufferBytesAvailable,
                *CACHE_WAIT_INSERT,
            ),
        };

        if wait_insert {
            let permit = buffer
                .clone()
                .acquire_many_owned(u32::try_from(bytes).unwrap())
                .await
                .expect("error from acquire_many_owned");
            self.stats
                .track_instantaneous(stat, (size - buffer.available_permits()) as u64);
            Some(permit)
        } else {
            // The permit should be dropped when the write to disk completes. It serves to limit the number
            // of insert()'s that we can buffer before dropping (ignoring) insertion requests.
            match buffer
                .clone()
                .try_acquire_many_owned(u32::try_from(bytes).unwrap())
            {
                Ok(permit) => {
                    self.stats
                        .track_instantaneous(stat, (size - buffer.available_permits()) as u64);
                    Some(permit)
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => None,
                Err(e) => panic!("unexpected error from try_acquire_many_owned: {:?}", e),
            }
        }
    }

    /// Initiates insertion of this block; doesn't wait for the write to disk.
    pub async fn insert(&self, locked_key: LockedKey, bytes: AlignedBytes, source: InsertSource) {
        // This permit will be dropped when the write to disk completes.  It
        // serves to limit the number of insert()'s that we can buffer before
        // dropping (ignoring) insertion requests.
        let insert_permit = match self.reserve_buffer_space(bytes.len(), source).await {
            Some(permit) => permit,
            None => {
                self.stats.track_count(InsertDropQueueFull);
                return;
            }
        };

        self.stats.track_bytes(InsertBytes, bytes.len() as u64);
        self.stats.track_count(match source {
            InsertSource::Heal => InsertForHealing,
            InsertSource::Read => InsertForRead,
            InsertSource::SpeculativeRead => InsertForSpeculativeRead,
            InsertSource::Write => InsertForWrite,
        });

        let state = self.state.clone();
        let write_slots = self.write_slots.clone();
        tokio::spawn(async move {
            // Get a permit to write to disk before waiting on the state lock.
            // This ensures that once we assign this insertion to a checkpoint,
            // the insertion will complete relatively quickly (e.g.
            // milliseconds).  This way, we don't have outstanding_writes that
            // take a long time to complete, preventing a checkpoint from making
            // progress.  Acquiring the WritePermit may take a long time,
            // because we have to wait for any in-progress insertions (up to
            // CACHE_INSERT_MAX_BUFFER) to complete before we can write to disk.
            let _write_permit = write_slots.acquire_owned().await.unwrap();

            // Now that we are ready to issue the write to disk, insert to the
            // cache in the current checkpoint (allocate a block, add to
            // pending_changes and outstanding_writes).
            let fut = lock_non_send(&state).await.insert(locked_key, bytes);
            fut.await;
            // We want to hold onto the insert_permit until the write completes
            // because it represents the memory that's required to buffer this
            // insertion, which isn't released until the io completes.
            // Similarly, the write_permit (roughly) represents the disks'
            // capacity to perform i/o.
            drop(insert_permit);
        });
    }

    pub async fn insert_all(
        &self,
        guid: PoolGuid,
        blocks: &HashMap<BlockId, Bytes>,
        source: InsertSource,
    ) {
        let pool_id = self.state.lock().await.map_pool_guid(guid);
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
                    .track_bytes(InsertDropQueueFull, blocks.len() as u64);
                return;
            }
        };

        let futures = FuturesUnordered::new();

        for (block, bytes) in blocks.iter() {
            let cache = self.clone();
            let block = *block;
            let aligned_bytes = AlignedBytes::from((*bytes).clone());
            let fut = async move {
                let key = IndexKey::new(pool_id, block);
                let locked_key = LockedKey(cache.outstanding_lookups.lock(key).await);

                // We need to check for presence in the cache even for
                // InsertSource::Write, where we expect to be writing a "new"
                // BlockId that's never been written before, because if the
                // system crashed or the pool was rewound, a BlockId that was
                // already persisted to the cache may be reused.

                let present = cache
                    .lookup_impl(&locked_key, LookupSource::Write, |_state, value| {
                        future::ready(value.is_some())
                    })
                    .await;

                if !present {
                    // Get a permit to write to disk before waiting on the state lock.
                    // This ensures that once we assign this insertion to a checkpoint,
                    // the insertion will complete relatively quickly (e.g.
                    // milliseconds).  This way, we don't have outstanding_writes that
                    // take a long time to complete, preventing a checkpoint from making
                    // progress.  Acquiring the WritePermit may take a long time,
                    // because we have to wait for any in-progress insertions (up to
                    // CACHE_INSERT_MAX_BUFFER) to complete before we can write to disk.
                    let _write_permit = cache.write_slots.acquire_owned().await.unwrap();
                    let len = aligned_bytes.len();

                    // Now that we are ready to issue the write to disk, insert to the
                    // cache in the current checkpoint (allocate a block, add to
                    // pending_changes and outstanding_writes).
                    let fut = lock_non_send(&cache.state)
                        .await
                        .insert(locked_key, aligned_bytes);
                    fut.await;

                    cache.stats.track_bytes(InsertBytes, len as u64);
                    cache.stats.track_count(match source {
                        InsertSource::Heal => InsertForHealing,
                        InsertSource::Read => InsertForRead,
                        InsertSource::SpeculativeRead => InsertForSpeculativeRead,
                        InsertSource::Write => InsertForWrite,
                    });
                }
            };
            with_alloctag_hf("ZettaCache::ingest_all FuturesUnordered.push()", || {
                futures.push(fut)
            });
        }
        tokio::spawn(async move {
            futures.for_each(|_| async {}).await;
            // We want to hold onto the insert_permit until the write completes
            // because it represents the memory that's required to buffer this
            // insertion, which isn't released until the io completes.
            drop(insert_permit);
        });
    }

    pub async fn heal(&self, guid: PoolGuid, block: BlockId, object_bytes: AlignedBytes) {
        if let LookupResponse::Present((cache_bytes, locked_key)) =
            self.lookup(guid, block, LookupSource::Write).await
        {
            // For (hopefully) obvious reasons, we only need to do the heal when the bytes contained in the cache differ
            // from the bytes contained in the object store. The bytes contained in the object store are always preferred
            // over the bytes contained in the cache; we assume the bytes passed were retrieved from the object store.
            if *cache_bytes != *object_bytes {
                self.stats.track_count(HealedBlocks);
                debug!("Healing cache: {:?}", locked_key.key());
                // Note: this will result in a second insert for the same key in the index. This will be resolved either
                // in the insert code (if the first insert is in pending_changes) or later during the next merge.
                self.insert(locked_key, object_bytes, InsertSource::Heal)
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

pub struct ZCacheDBHandle {
    block_access: Arc<BlockAccess>,
    primary: PrimaryPhys,
    primary_disk: DiskId,
    guid: u64,
    checkpoint: Arc<ZettaCheckpointPhys>,
    extent_allocator: Arc<ExtentAllocator>,
}

impl ZCacheDBHandle {
    pub async fn dump_superblocks(paths: Vec<&str>) -> Result<()> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in paths {
            match Disk::new(path, true) {
                Ok(disk) => disks.push(disk),
                Err(err) => writeln_stderr!("error: {}", err),
            }
        }
        if disks.is_empty() {
            return Ok(());
        }
        let block_access = BlockAccess::new(disks, true);
        SuperblockPhys::dump_all(&block_access).await;
        Ok(())
    }

    pub async fn open(paths: Vec<&str>) -> Result<ZCacheDBHandle> {
        let mut disks: Vec<Disk> = Vec::with_capacity(paths.len());
        for path in paths {
            disks.push(Disk::new(path, true)?);
        }
        let block_access = Arc::new(BlockAccess::new(disks, true));

        let (primary, primary_disk, guid, _extra_disks) = PrimaryPhys::read(&block_access).await?;
        let checkpoint =
            Arc::new(ZettaCheckpointPhys::read(&block_access, primary.checkpoint).await);

        let mut builder = ExtentAllocatorBuilder::new(&checkpoint.extent_allocator);
        // We should be able to get away without claiming the metadata space,
        // since we aren't allocating anything, but we may also want to do this
        // for verification (e.g. that there aren't overlapping Extents).
        checkpoint.claim(&mut builder);
        let extent_allocator = Arc::new(ExtentAllocator::open(builder));

        Ok(ZCacheDBHandle {
            block_access,
            primary,
            primary_disk,
            guid,
            checkpoint,
            extent_allocator,
        })
    }

    pub async fn dump_free_space(&self) {
        writeln_stdout!("Superblock");
        writeln_stdout!("  Primary {:?}, GUID: {}", self.primary_disk, self.guid);
        writeln_stdout!();

        writeln_stdout!("Checkpoint Region");
        writeln_stdout!("  {:?}", self.primary.checkpoint_capacity);
        writeln_stdout!(
            "  checkpoint: {} used out of {} ({:.1}%, must be <50%)",
            nice_p2size(self.primary.checkpoint.size),
            nice_p2size(self.primary.checkpoint_capacity.size),
            self.primary.checkpoint.size as f64 * 100.0
                / self.primary.checkpoint_capacity.size as f64
        );
        writeln_stdout!();

        writeln_stdout!("Old Checkpoint Regions");
        let mut unused_checkpoint_space = 0;
        for region in self.primary.old_checkpoint_capacity.iter() {
            unused_checkpoint_space += region.size;
            writeln_stdout!("  {:?}", region);
        }
        writeln_stdout!("  ----------------------");
        writeln_stdout!("  total: {}", nice_p2size(unused_checkpoint_space));
        writeln_stdout!();

        writeln_stdout!("Metadata Region");
        let mut total_used_bytes = 0;
        let mut total_allocated_bytes = 0;
        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "operation log",
            nice_p2size(self.checkpoint.operation_log.bytes()),
            nice_p2size(self.checkpoint.operation_log.capacity_bytes())
        );
        total_used_bytes += self.checkpoint.operation_log.bytes();
        total_allocated_bytes += self.checkpoint.operation_log.capacity_bytes();

        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "spacemap",
            nice_p2size(self.checkpoint.block_allocator.spacemap_bytes()),
            nice_p2size(self.checkpoint.block_allocator.spacemap_capacity_bytes())
        );
        total_used_bytes += self.checkpoint.block_allocator.spacemap_bytes();
        total_allocated_bytes += self.checkpoint.block_allocator.spacemap_capacity_bytes();

        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "spacemap_next",
            nice_p2size(self.checkpoint.block_allocator.spacemap_next_bytes()),
            nice_p2size(
                self.checkpoint
                    .block_allocator
                    .spacemap_next_capacity_bytes()
            )
        );
        total_used_bytes += self.checkpoint.block_allocator.spacemap_next_bytes();
        total_allocated_bytes += self
            .checkpoint
            .block_allocator
            .spacemap_next_capacity_bytes();

        writeln_stdout!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "index log",
            nice_p2size(self.checkpoint.old_index.log_bytes()),
            nice_p2size(self.checkpoint.old_index.log_capacity_bytes())
        );
        total_used_bytes += self.checkpoint.old_index.log_bytes();
        total_allocated_bytes += self.checkpoint.old_index.log_capacity_bytes();

        if let Some(progress) = self.checkpoint.merge_progress.clone() {
            writeln_stdout!(
                "  {:>13} - {:>6} used out of {:>6} allocated",
                "progress log",
                nice_p2size(progress.operation_log.bytes()),
                nice_p2size(progress.operation_log.capacity_bytes())
            );
            total_used_bytes += progress.operation_log.bytes();
            total_allocated_bytes += progress.operation_log.capacity_bytes();
            writeln_stdout!(
                "  {:>13} - {:>6} used out of {:>6} allocated",
                "progress index",
                nice_p2size(progress.new_index.log_bytes()),
                nice_p2size(progress.new_index.log_capacity_bytes())
            );
            total_used_bytes += progress.new_index.log_bytes();
            total_allocated_bytes += progress.new_index.log_capacity_bytes();
        }
        writeln_stdout!("  ----------------------");
        let metadata_region_size = self
            .checkpoint
            .extent_allocator
            .capacity
            .iter()
            .map(|extent| extent.size)
            .sum();
        writeln_stdout!(
            "  {:>13} - {} ({:.1}%) used, {} ({:.1}%) allocated out of {:>6} total",
            "total",
            nice_p2size(total_used_bytes),
            total_used_bytes as f64 * 100.0 / metadata_region_size as f64,
            nice_p2size(total_allocated_bytes),
            total_allocated_bytes as f64 * 100.0 / metadata_region_size as f64,
            nice_p2size(metadata_region_size)
        );
        writeln_stdout!("  ----------------------");
        for (disk, (used, total)) in self.extent_allocator.zcachedb_metadata_per_disk() {
            writeln_stdout!(
                "  {:?} - {:>6} allocated out of {:>6} total",
                disk,
                nice_p2size(used),
                nice_p2size(total)
            );
        }
        writeln_stdout!();

        let balloc_size = self
            .checkpoint
            .block_allocator
            .capacity()
            .iter()
            .map(|extent| extent.size)
            .sum();
        writeln_stdout!("{:>6} User Data Region", nice_p2size(balloc_size));
    }

    pub async fn dump_structures(&self, opts: DumpStructuresOptions) {
        if opts.dump_defaults {
            writeln_stdout!("{:#?}", self.primary);
            writeln_stdout!("{:#?}", self.checkpoint);
        }

        if opts.dump_atime_histogram {
            writeln_stdout!("DUMP INDEX ATIME HISTOGRAM");
            writeln_stdout!("{}", self.checkpoint.old_index.atime_histogram());

            if let Some(progress) = &self.checkpoint.merge_progress {
                writeln_stdout!("DUMP MERGE INDEX ATIME HISTOGRAM");
                writeln_stdout!("{}", progress.new_index.atime_histogram());
            }
        }

        if opts.dump_spacemaps {
            zcachedb_dump_spacemaps(
                self.checkpoint.block_allocator.clone(),
                self.block_access.clone(),
                self.extent_allocator.clone(),
            )
            .await;
        }

        if opts.dump_operation_log_raw {
            self.checkpoint
                .operation_log
                .iter_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;
            if let Some(mpp) = &self.checkpoint.merge_progress {
                writeln_stdout!("\nold operation log from MergeProgressPhys:");
                mpp.operation_log
                    .iter_chunks(self.block_access.clone())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
            }
        }

        if opts.dump_index_log_raw {
            self.checkpoint
                .old_index
                .iter_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;

            self.checkpoint
                .old_index
                .iter_summary_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    writeln_stdout!("{:#?}", chunk);
                })
                .await;

            if let Some(mpp) = &self.checkpoint.merge_progress {
                writeln_stdout!("\nnew index from MergeProgressPhys:");
                mpp.new_index
                    .iter_chunks(self.block_access.clone())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
                mpp.new_index
                    .iter_summary_chunks(self.block_access.clone())
                    .for_each(|chunk| async move {
                        writeln_stdout!("{:#?}", chunk);
                    })
                    .await;
            }
        }

        if opts.dump_rebalance_log_raw {
            if let Some(progress) = &self.checkpoint.merge_progress {
                if let Some(log) = progress.rebalance_log.as_ref() {
                    log.iter_chunks(self.block_access.clone())
                        .for_each(|chunk| async move {
                            writeln_stdout!("{:#?}", chunk);
                        })
                        .await;
                }
            }
        }
    }

    pub async fn dump_slabs(&self, opts: DumpSlabsOptions) {
        zcachedb_dump_slabs(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            self.checkpoint.block_allocator.clone(),
            opts,
        )
        .await;
    }

    pub async fn verify_index(&self) {
        writeln_stdout!("iterating current (old) index to verify histogram...");
        self.checkpoint
            .old_index
            .verify_histogram(self.block_access.clone())
            .await;

        if let Some(mpp) = &self.checkpoint.merge_progress {
            writeln_stdout!("iterating merge (new) index to verify histogram...");
            mpp.new_index
                .verify_histogram(self.block_access.clone())
                .await;
        }
        writeln_stdout!("histograms correct");
    }
}

pub struct ValidIndexValue(IndexValue);

impl ValidIndexValue {
    pub fn extent(&self) -> Extent {
        self.0.extent().unwrap()
    }
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

    fn ghost_hit_check(&mut self, value: IndexValue, source: LookupSource) {
        let (live_cutoff, ghost_cutoff) = match &self.merge {
            Some(ms) => (ms.eviction_cutoff, ms.ghost_cutoff),
            None => (
                self.atime_histogram.first_live(),
                self.atime_histogram.first_ghost(),
            ),
        };

        if value.atime() >= ghost_cutoff
            && value.atime() < live_cutoff
            && matches!(source, LookupSource::Read)
        {
            // This is a hit in the ghost hit-by-size histogram
            let size = self.atime_histogram.size_at(value.atime());
            self.size_histogram.ghost_hit(size);
        }
    }

    /// Returns the Id (index) associated with the pool GUID.
    /// If not found, the GUID is added to the known set and a new Id generated.
    fn map_pool_guid(&mut self, guid: PoolGuid) -> PoolId {
        // XXX - this is an O(n) algorithm, which is fine for a small number of pools,
        // but we may want to use a hashmap for this if there are lots of pools.
        for (id, mapped_guid) in self.pool_guids.iter().enumerate() {
            if *mapped_guid == guid {
                return PoolId(u8::try_from(id).unwrap());
            }
        }
        let id = u8::try_from(self.pool_guids.len()).unwrap();
        debug!("New {:?} added with {:?}", guid, PoolId(id));
        self.pool_guids.push(guid);
        PoolId(id)
    }

    fn lookup_with_value_from_index(
        &mut self,
        key: &IndexKey,
        value_from_index: IndexValue,
        source: LookupSource,
    ) -> Option<ValidIndexValue> {
        // Note: we're here because there was no PendingChange for this key, but
        // since we dropped the lock, a PendingChange may have been inserted
        // since then.  So we need to check for a PendingChange before using the
        // value from the index.
        // XXX is this still true, given that now we have the LockedKey
        // (outstanding_lookups lock)?
        let value = match self.pending_changes.get(key) {
            Some(PendingChange::Insert(value_ref))
            | Some(PendingChange::UpdateAtime(UpdateAtime(value_ref, _))) => *value_ref,
            None => value_from_index,
        };
        self.ghost_hit_check(value, source);
        self.validate(value)
    }

    fn lookup(
        &mut self,
        locked_key: &LockedKey,
        valid_value: ValidIndexValue,
        source: LookupSource,
    ) -> impl Future<Output = Option<AlignedBytes>> {
        let mut value = valid_value.0;
        let key = locked_key.key();
        super_trace!("cache hit: reading {:?} from {:?}", key, value);

        if matches!(source, LookupSource::Read) {
            // Add an entry to the hit-by-size histogram
            let size = self.atime_histogram.size_at(value.atime());
            super_trace!("cache size {} at {:?}", size, value.atime());
            self.size_histogram.live_hit(size);
        }
        let original_atime = value.atime();
        if original_atime != self.atime {
            // Update the atime histogram
            self.atime_histogram.remove(value);
            value = IndexValue::new(value.location(), value.size(), self.atime);
            self.atime_histogram.insert(value);
        }

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
                    // Perserve the original atime (from the Index) in case we "replace" this block and
                    // need to reset the histogram for the orignal block (i.e. when we find the old block
                    // during the merge, we can decrement the atime histogram)
                    super_trace!(
                        "adding PendingChanges::UpdateAtime({:?}) for {:?}",
                        value,
                        key
                    );
                    with_alloctag(Self::PENDING_CHANGES_TAG, || {
                        ve.insert(PendingChange::UpdateAtime(UpdateAtime(
                            value,
                            original_atime,
                        )))
                    });
                    self.update_pending_stats();
                }
            }
            btree_map::Entry::Occupied(mut oe) => match oe.get_mut() {
                PendingChange::Insert(value_ref)
                | PendingChange::UpdateAtime(UpdateAtime(value_ref, _)) => {
                    *value_ref = value;
                }
            },
        }

        // Note: it's unlikely but possible that this lookup is for a block that
        // was just inserted, and whose write has not yet completed.  In this
        // case we may read stale data from disk.  The real fix would be to know
        // which Atime's have been persisted by the last checkpoint, and if the
        // entry is too new, to treat it as a cache miss.

        // There can't be a write lock on the outstanding_reads because it's
        // only held for write when the state lock is also held, and we have the
        // state lock.
        let read_permit = self.outstanding_reads.clone().try_read_owned().unwrap();

        let block_access = self.block_access.clone();

        async move {
            let bytes = block_access
                .read_raw(valid_value.extent(), DiskIoType::ReadDataForLookup)
                .await;

            // It's now OK for a checkpoint to complete, potentially freeing this block.
            drop(read_permit);

            // XXX we can easily handle an io error here by returning None
            Some(bytes)
        }
    }

    fn update_pending_stats(&self) {
        let old_pending = match &self.merge {
            Some(ms) => ms.old_pending_changes.len() as u64,
            None => 0,
        };
        self.stats.track_instantaneous(
            PendingChanges,
            old_pending + self.pending_changes.len() as u64,
        );
    }

    /// Insert this block to the cache, if space and performance parameters
    /// allow.  It may be a recent cache miss, or a recently-written block.
    /// Returns a Future to be executed after the state lock has been dropped.
    fn insert(&mut self, locked_key: LockedKey, bytes: AlignedBytes) -> impl Future {
        let noop = future::Either::Left(async {});
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
            return noop;
        }

        let buf_size = bytes.len();
        let location = match self.allocate_block(u32::try_from(buf_size).unwrap()) {
            Some(location) => location,
            None => return noop,
        };

        // XXX if this is past the last block of the main index, we can write it
        // there (and location_dirty:false) instead of logging it

        let key = locked_key.key();
        let value = IndexValue::new(Some(location), u32::try_from(buf_size).unwrap(), self.atime);

        if let Some(pc) = with_alloctag(Self::PENDING_CHANGES_TAG, || {
            self.pending_changes
                .insert(key, PendingChange::Insert(value))
        }) {
            debug!(
                "Inserting {:?} over existing entry {:?}, should be heal",
                value, pc
            );
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
        self.update_pending_stats();

        super_trace!("adding Insert to operation_log {:?} {:?}", key, value);
        self.operation_log
            .push(OperationLogEntry::Insert(key, value));

        let write_permit = self.outstanding_writes.clone().try_read_owned().unwrap();

        let block_access = self.block_access.clone();
        // Note: locked_key can be dropped before the i/o completes, since the
        // changes to the State have already been made.
        future::Either::Right(async move {
            block_access
                .write_raw(location, bytes, DiskIoType::WriteDataForInsert)
                .await;

            // It's now OK for a checkpoint to complete, persisting the index
            // entry that references this block.
            drop(write_permit);
        })
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
    async fn flush_checkpoint(&mut self, old_index: IndexRunPhys, new_index: Option<IndexRunPhys>) {
        debug!(
            "flushing checkpoint {:?}",
            self.primary.checkpoint_id.next()
        );
        self.stats
            .track_instantaneous(BlockAllocatorSize, self.block_allocator.size());
        self.stats
            .track_instantaneous(BlockAllocatorAvailable, self.block_allocator.available());
        self.stats.track_instantaneous(
            BlockAllocatorFreeSlabsSize,
            self.block_allocator.free_slabs_size(),
        );

        let begin_checkpoint = Instant::now();

        // Wait for all outstanding reads, so that if we free the space they are
        // reading, it can't be overwritten until after the read completes.
        let begin = Instant::now();
        self.outstanding_reads.write().await;
        debug!(
            "waited for outstanding_reads in {}ms",
            begin.elapsed().as_millis()
        );

        // Wait for all outstanding writes, so that if we crash, the blocks
        // referenced by the index/operation_log will actually have the correct
        // contents.
        let begin = Instant::now();
        self.outstanding_writes.write().await;
        debug!(
            "waited for outstanding_writes in {}ms",
            begin.elapsed().as_millis()
        );

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

        let checkpoint = ZettaCheckpointPhys {
            generation: self.primary.checkpoint_id.next(),
            pool_guids: self.pool_guids.clone(),
            extent_allocator: self.extent_allocator.get_phys(),
            old_index,
            operation_log: operation_log_phys,
            last_atime: self.atime,
            block_allocator: self.block_allocator.flush().await,
            size_histogram: self.size_histogram.clone(),
            merge_progress: merge_progress_phys,
        };

        // There may be some metadata space allocated but not yet part of the
        // checkpoint, because the merge task runs concurrently with this and
        // may have allocated an extent which it hasn't yet told the checkpoint
        // about.  However, the amount of this space should be limited.  If
        // there's a large amount of space that's not accounted for in the
        // checkpoint, it likely indicates a leak, e.g. a BlockBasedLog is no
        // longer in the Checkpoint but wasn't .clear()'ed.
        {
            let mut checkpoint_extents = ExtentAllocatorBuilder::new(&checkpoint.extent_allocator);
            checkpoint.claim(&mut checkpoint_extents);
            let checkpoint_bytes = checkpoint_extents.allocatable_bytes();
            let allocator_bytes = self.extent_allocator.allocatable_bytes();
            if allocator_bytes + *DEFAULT_EXTENT_SIZE * 4 < checkpoint_bytes {
                warn!("possible leak of metadata space: {} available according to checkpoint but not in memory",
                    nice_p2size(checkpoint_bytes - allocator_bytes));
            }
        }

        let raw = self
            .block_access
            .chunk_to_raw(EncodeType::Json, &checkpoint);

        let mut checkpoint_extent = if self
            .primary
            .checkpoint_capacity
            .contains(&self.primary.checkpoint)
        {
            // Try placing checkpoint after current checkpoint.
            Extent::new(
                self.primary.checkpoint.location.disk(),
                self.primary.checkpoint.location.offset() + self.primary.checkpoint.size,
                raw.len() as u64,
            )
        } else {
            // The checkpoint region has moved. Write new checkpoint in new region.
            self.primary.checkpoint_capacity.range(0, raw.len() as u64)
        };

        if !self
            .primary
            .checkpoint_capacity
            .contains(&checkpoint_extent)
        {
            // Out of space; go back to the beginning of the checkpoint space.
            checkpoint_extent.location = self.primary.checkpoint_capacity.location;
            assert!(self
                .primary
                .checkpoint_capacity
                .contains(&checkpoint_extent));
            assert_le!(
                checkpoint_extent.location.offset() + checkpoint_extent.size,
                self.primary.checkpoint.location.offset()
            );
            // XXX The above assertion could fail if there isn't enough
            // checkpoint space for 3 checkpoints (the existing one that
            // we're writing before, the one we're writing, and the space
            // after the existing one that we aren't using).  Note that we
            // could in theory reduce this to 2 checkpoints if we allowed a
            // single checkpoint to wrap around (part of it at the end and
            // then part at the beginning of the space).
        }
        maybe_die_with(|| format!("before writing {:#?}", checkpoint));
        debug!("writing to {:?}: {:#?}", checkpoint_extent, checkpoint);

        self.block_access
            .write_raw(
                checkpoint_extent.location,
                raw,
                DiskIoType::MaintenanceWrite,
            )
            .await;

        self.primary.checkpoint = checkpoint_extent;
        self.primary.checkpoint_id = self.primary.checkpoint_id.next();
        self.primary.feature_flags = SUPPORTED_FEATURES.keys().cloned().collect();
        // We need to write all the disks' superblocks in case new disks have
        // been added.
        self.primary
            .write_all(self.primary_disk, self.guid, &self.block_access)
            .await;

        self.extent_allocator.checkpoint_done();

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
        // much of a buffer here. We use 100 because we might accumulate some messages while actually
        // flushing out the checkpoint.
        let (index_tx, checkpoint_rx) = mpsc::channel(100);
        let (merge_tx, index_rx) = mpsc::channel(100);

        let block_access = self.block_access.clone();
        let spawn_merge = merge.clone();
        let start_key = next_index.last_key();

        tokio::spawn(async move {
            spawn_merge
                .merge_task(merge_tx, old_index, start_key, &block_access)
                .await;
        });

        tokio::spawn(async move {
            merge
                .next_index_task(index_rx, index_tx.clone(), &mut next_index)
                .await;

            // We drop this before sending the Complete message, so that rotate_index() can unwrap the Arc.
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
            self.extent_allocator.clone(),
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
                    .iter(self.block_access.clone())
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

    /// Start a new merge task if there are enough pending changes
    async fn try_start_merge_task(
        &mut self,
        old_index: Arc<tokio::sync::RwLock<IndexRun>>,
    ) -> Option<(mpsc::Receiver<MergeMessage>, IndexRunPhys)> {
        if self.pending_changes.len() < self.pending_changes_trigger
            && self.block_allocator.size() - self.block_allocator.available()
                < (self.block_allocator.size() / 100) * *HIGH_WATER_CACHE_SIZE_PCT
            && !self.block_allocator.rebalance_needed()
        {
            trace!(
                "not starting new merge, only {} pending changes",
                self.pending_changes.len()
            );
            return None;
        }

        let used = self.block_allocator.size() - self.block_allocator.available();
        let target_size = (self.block_allocator.size() / 100) * *TARGET_CACHE_SIZE_PCT;
        let target_reduction = used.checked_sub(target_size).unwrap_or_default();
        info!(
            "target cache size for storage size {}GB is {}GB; {}MB used; {}MB high-water; {}MB freeing; histogram covers {}MB",
            self.block_allocator.size() / 1024 / 1024 / 1024,
            target_size / 1024 / 1024 / 1024,
            used / 1024 / 1024,
            (self.block_allocator.size() / 100) * *HIGH_WATER_CACHE_SIZE_PCT / 1024 / 1024,
            self.block_allocator.freeing() / 1024 / 1024,
            self.atime_histogram.sum_live() / 1024 / 1024,
        );
        self.stats
            .track_instantaneous(BlockAllocatorSize, self.block_allocator.size());
        self.stats
            .track_instantaneous(BlockAllocatorAvailable, self.block_allocator.available());
        self.stats.track_instantaneous(
            BlockAllocatorFreeSlabsSize,
            self.block_allocator.free_slabs_size(),
        );

        let eviction_atime = self
            .atime_histogram
            .atime_for_eviction_target(target_reduction);

        let ghost_size = self.atime_histogram.sum_ghost() + target_reduction;
        let ghost_target = (self.block_allocator.size() / 100) * *GHOST_CACHE_SIZE_PCT;
        let ghost_reduction = ghost_size.checked_sub(ghost_target).unwrap_or_default();
        let ghost_atime = self.atime_histogram.atime_for_ghost_target(ghost_reduction);
        debug!(
            "ghost history size: {} (including {} transfering from live), target size: {}, removing {}",
            nice_p2size(ghost_size),
            nice_p2size(target_reduction),
            nice_p2size(ghost_target),
            nice_p2size(ghost_reduction),
        );

        let old_operation_log_phys = self.operation_log.flush().await;

        // Create an empty operation log that is consistent with the empty pending state.
        // Note that we don't want to just clear the existing operation log, since we are
        // still preserving that in the merging state.
        self.operation_log = BlockBasedLog::open(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            Default::default(),
        );
        let next_index_phys = IndexRunPhys::new(ghost_atime, eviction_atime);
        let next_index = IndexRun::open(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            next_index_phys.clone(),
        )
        .await;

        let rebalance = match self.block_allocator.rebalance_init() {
            None => None,
            Some(map) => {
                let mut log = BlockBasedLog::<RebalanceLogEntry>::open(
                    self.block_access.clone(),
                    self.extent_allocator.clone(),
                    Default::default(),
                );

                for (&old, &new) in map.iter() {
                    log.push(RebalanceLogEntry { old, new });
                }

                let log_phys = log.flush().await;

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
        merge.old_operation_log_phys.clear(&self.extent_allocator);

        if let Some(rebalance) = &merge.rebalance {
            let begin = Instant::now();

            let mut evicted_keys = Vec::new();
            for (key, pc) in self.pending_changes.iter_mut() {
                match pc {
                    PendingChange::Insert(value) => {
                        // Inserts in the "pending changes" list will never be moved as part of a rebalance.
                        let extent = value.extent().unwrap();
                        assert_eq!(rebalance.remap(extent).unwrap(), extent.location);
                    }
                    PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                        // If a lookup occurs on a block that is being moved as part of rebalancing, the lookup will
                        // return the "old" location of the block (which is valid while we are merging) and will be
                        // stored in an UpdateAtime record in pending_changes. Now that the merge is complete, we need
                        // to either: 1) remap these old locations to their "new" rebalanced locations, or 2) remove
                        // the UpdateAtime due to rebalancing having had to evict the entry from the cache (i.e. due
                        // to an allocation failure when attempting to allocate the new disk location).
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

        let begin = Instant::now();

        // Populate index_cache with old_pending_changes
        for (key, pc) in &merge.old_pending_changes {
            match pc {
                PendingChange::Insert(value)
                | PendingChange::UpdateAtime(UpdateAtime(value, _)) => {
                    // For the "old pending changes" list, we need to not only do the remap for atime updates, but also
                    // for inserts. This is because an insert could have occurred just prior to the merge starting, and
                    // then the location for that new insert may have been rebalanced via the merge. In this case, we need
                    // to ensure index cache is populated correctly with the new location(s).
                    match self.validate(*value) {
                        Some(_) => {
                            let remapped = match merge.rebalance.as_ref() {
                                Some(rebalance) => {
                                    rebalance.remap(value.extent().unwrap()).map(|location| {
                                        IndexValue::new(Some(location), value.size(), value.atime())
                                    })
                                }
                                None => Some(*value),
                            };

                            match remapped {
                                Some(value) => with_alloctag("ZettaCacheState.index_cache", || {
                                    self.index_cache.put(*key, value);
                                }),
                                None => {
                                    self.index_cache.pop(key);
                                }
                            }
                        }
                        None => continue,
                    }
                }
            }
        }

        debug!(
            "took {}ms to add old pending changes with {} entries to index cache",
            begin.elapsed().as_millis(),
            merge.old_pending_changes.len()
        );

        if let Some(mut rebalance) = merge.rebalance {
            // Free up the extents that have been allocated for the cache rebalancing
            rebalance.log_phys.clear(&self.extent_allocator);
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
            cache_capacity + cache_capacity / 100 * *GHOST_CACHE_SIZE_PCT,
            cache_capacity,
            cache_capacity - self.block_allocator.size(),
            *QUANTILES_IN_SIZE_HISTOGRAM,
        )
    }
}
