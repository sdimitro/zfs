use crate::atime_histogram::AtimeHistogramPhys;
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
use serde::{Deserialize, Serialize};
use std::collections::btree_map;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::mem;
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use sysinfo::System;
use sysinfo::SystemExt;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::time::{sleep_until, timeout_at};
use util::get_tunable;
use util::maybe_die_with;
use util::nice_p2size;
use util::super_trace;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::DiskIoType;
use util::zettacache_stats::*;
use util::AlignedBytes;
use util::From64;
use util::LockSet;
use util::LockedItem;
use util::MutexExt;
use uuid::Uuid;

lazy_static! {
    static ref DEFAULT_CHECKPOINT_SIZE_PCT: f64 = get_tunable("default_checkpoint_size_pct", 0.1);
    static ref DEFAULT_METADATA_SIZE_PCT: f64 = get_tunable("default_metadata_size_pct", 15.0); // Can lower this to test forced eviction.
    static ref MAX_PENDING_CHANGES: usize = get_tunable("max_pending_changes", 50_000); // XXX should be based on RAM usage, ~tens of millions at least
    static ref CHECKPOINT_INTERVAL: Duration = Duration::from_secs(get_tunable("checkpoint_interval_secs", 60));
    static ref MERGE_PROGRESS_MESSAGE_INTERVAL: Duration = Duration::from_millis(get_tunable("merge_progress_message_interval_ms", 1000));
    static ref MERGE_PROGRESS_CHECK_COUNT: u32 = get_tunable("merge_progress_check_count", 100);
    static ref TARGET_CACHE_SIZE_PCT: u64 = get_tunable("target_cache_size_pct", 80);
    static ref HIGH_WATER_CACHE_SIZE_PCT: u64 = get_tunable("high_water_cache_size_pct", 82);
    static ref GHOST_CACHE_SIZE_PCT: u64 = get_tunable("ghost_cache_size_pct", 100);
    static ref QUANTILES_IN_SIZE_HISTOGRAM: usize = get_tunable("quantiles_in_size_histogram", 100);
    static ref CACHE_INSERT_BLOCKING_BUFFER_BYTES: usize = get_tunable("cache_insert_blocking_buffer_bytes", 256 * 1024 * 1024);
    static ref CACHE_INSERT_NONBLOCKING_BUFFER_BYTES: usize = get_tunable("cache_insert_nonblocking_buffer_bytes", 256 * 1024 * 1024);
    static ref INDEX_CACHE_ENTRIES_MEM_PCT: usize = get_tunable("index_cache_entries_mem_pct", 10);
    static ref DISK_EXPAND_MIN_PCT: f64 = get_tunable("disk_expand_min_pct", 10.0);

    // A limit of 8 should be enough to get to the 16,000 IOPS limit of medium-size instances/disks on gp3; because gp3 has ~1ms latency for each operation,
    // and each closure this limit applies to, performs 2 operations (one read, and one write). Additionally, this is half the limit of outstanding writes.
    static ref CACHE_REBALANCE_CONCURRENCY_LIMIT: usize = get_tunable("cache_rebalance_concurrency_limit", 8);

    // Debug tunable to exercise the explicit eviction code path (normally only used by heal()). A value of 0 means, never evict on cache lookup;
    // a value of 1 means, evict every other lookup; a value of 2 means, evict every third lookup; etc.
    static ref CACHE_EVICT_EACH_N_LOOKUPS: usize = get_tunable("cache_evict_each_n_lookups", 0);
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MergeProgressPhys {
    rebalance_log: Option<BlockBasedLogPhys<RebalanceLogEntry>>,
    operation_log: BlockBasedLogPhys<OperationLogEntry>,
    index: ZettaCacheIndexPhys,
}

#[derive(Serialize, Deserialize, Debug)]
struct ZettaCheckpointPhys {
    generation: CheckpointId,
    extent_allocator: ExtentAllocatorPhys,
    block_allocator: BlockAllocatorPhys,
    last_atime: Atime,
    index: ZettaCacheIndexPhys,
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
        self.index.claim(builder);
        self.operation_log.claim(builder);
        if let Some(progress) = self.merge_progress.as_ref() {
            progress.operation_log.claim(builder);
            progress.index.claim(builder);
            if let Some(rebalance_log) = progress.rebalance_log.as_ref() {
                rebalance_log.claim(builder);
            }
        }
    }
}

/// A PendingChange is the in-core data structure for tracking changes to the index between merges.
/// Four types of events are tracked: insertions, removals, lookup hits (atime update), and removals
/// followed by insertions. The last change type is necessary because, otherwise, the change would
/// look like an insertion on top of an existing index entry. Note that removal does not need to store
/// a value (disk location) and atime updates store an extra item: the original atime of the entry in
/// the index. This atime is necessary when recording a removal operation in the persistent operation
/// log. The operation log does not track atime updates so, when a log entry for a remove is logged,
/// the "original" atime for a remove from cache entry needs to be recorded to maintain consistency
/// on log replay when the cache is reopened.
#[derive(Debug, Clone, Copy)]
enum PendingChange {
    Insert(IndexValue),
    UpdateAtime(IndexValue, Atime),
    Remove(),
    RemoveThenInsert(IndexValue),
}

#[derive(Clone)]
pub struct ZettaCache {
    block_access: Arc<BlockAccess>,

    // lock ordering: index first then state
    index: Arc<tokio::sync::RwLock<ZettaCacheIndex>>,
    // XXX may need to break up this big lock.  At least we aren't holding it while doing i/o
    state: Arc<tokio::sync::Mutex<ZettaCacheState>>,
    outstanding_lookups: LockSet<IndexKey>,
    stats: Arc<CacheStats>,
    timebase: Instant, // used when collecting stats
    blocking_buffer_bytes_available: Arc<Semaphore>,
    nonblocking_buffer_bytes_available: Arc<Semaphore>,
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
    Remove(IndexKey, IndexValue),
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

#[derive(Debug, Serialize, Deserialize)]
struct MergeProgress {
    new_index: ZettaCacheIndexPhys,
    free_list: Vec<Extent>,
}

#[derive(Debug)]
enum MergeMessage {
    Progress(MergeProgress),
    Complete(ZettaCacheIndex),
}

impl MergeMessage {
    /// Compose a progress update to send to the checkpoint task.
    async fn new_progress(next_index: &mut ZettaCacheIndex, free_list: Vec<Extent>) -> Self {
        let timer = Instant::now();
        let free_count = free_list.len();
        let message = MergeProgress {
            new_index: next_index.flush().await,
            free_list,
        };
        debug!("sending progress: index with {} entries ({}MB) last is {:?} flushed in {}ms, and {} frees",
            next_index.log.len(),
            next_index.log.num_bytes() / 1024 / 1024,
            next_index.last_key, timer.elapsed().as_millis(),
            free_count,);
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

                        return Some(DiskLocation {
                            disk: new_location.disk,
                            offset: new_location.offset + offset,
                        });
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
    old_pending_changes: BTreeMap<IndexKey, PendingChange>,
    old_operation_log_phys: BlockBasedLogPhys<OperationLogEntry>,
    eviction_cutoff: Atime,
    ghost_cutoff: Atime,
    stats: Arc<CacheStats>,
}

impl MergeState {
    fn map_index_entry_to_rebalanced_location(&self, mut entry: IndexEntry) -> Option<IndexEntry> {
        match self.rebalance.as_ref() {
            // If the data for an entry has been moved, due a cache rebalance operation, we need to update
            // the entry using the new location for the data. This way, once the cache commits to this new
            // index (containing this new entry), the old location for this entries data will no longer be
            // referenced (and thus, that location can be freed).
            Some(rebalance) => rebalance
                .remap(entry.value.extent().unwrap())
                .map(|location| {
                    entry.value.location = Some(location);
                    entry
                }),
            None => Some(entry),
        }
    }

    /// Add an entry to the new index, or evict it if its atime is before the eviction cut off.
    fn add_to_index_or_evict(
        &self,
        entry: IndexEntry,
        index: &mut ZettaCacheIndex,
        free_list: &mut Vec<Extent>,
    ) {
        if entry.value.atime >= self.eviction_cutoff {
            // If None, we don't add this entry to the free list, as it'll be freed automatically via rebalance_fini().
            if let Some(entry) = self.map_index_entry_to_rebalanced_location(entry) {
                index.append(entry);
            }
        } else {
            let mut ghost_entry = entry;
            if ghost_entry.value.location.is_some() {
                // If None, we don't add this entry to the free list, as it'll be freed automatically via rebalance_fini().
                if let Some(entry) = self.map_index_entry_to_rebalanced_location(ghost_entry) {
                    // This is a new ghost entry, free and strip old location infomation
                    free_list.push(entry.value.extent().unwrap());
                }
                ghost_entry.value.location = None;
                self.stats.track_count(Evictions);
            }
            if ghost_entry.value.atime >= self.ghost_cutoff {
                // Preserve ghost entry to our ghost history
                index.append(ghost_entry);
            } else {
                // Entry is now gone from Index, update our traversal postion
                index.update_last_key(ghost_entry.key);
            }
        }
    }

    /// This function runs in an async task to merge a set of pending changes with the current on-disk
    /// index in order to produce a new up-to-date on-disk index. It sends periodic "progress updates"
    /// (including block frees) to the checkpoint task.
    async fn merge_task(
        &self,
        tx: tokio::sync::mpsc::Sender<MergeMessage>,
        old_index: Arc<tokio::sync::RwLock<ZettaCacheIndex>>,
        next_index: &mut ZettaCacheIndex,
        block_access: &BlockAccess,
    ) {
        // We don't currently support concurrent free()'s while the rebalance is in-progress. Thus, we
        // need to do the rebalance first, prior to moving forward with the merge.
        self.rebalance(block_access).await;

        let begin = Instant::now();
        let old_index = old_index.read().await;
        info!(
            "writing new index to merge {} pending changes into index of {} entries ({} MB), eviction cutoff {:?}, ghost cutoff {:?}",
            self.old_pending_changes.len(),
            old_index.log.len(),
            old_index.log.num_bytes() / 1024 / 1024,
            self.eviction_cutoff,
            self.ghost_cutoff,
        );

        let mut free_list: Vec<Extent> = Vec::new();
        let mut timer = Instant::now();

        let start_key = next_index.last_key;
        debug!("using {:?} as start key for merge", start_key);
        let mut pending_changes_iter = self
            .old_pending_changes
            .range((start_key.map_or(Unbounded, Excluded), Unbounded))
            .peekable();

        let mut index_stream = Box::pin(old_index.log.iter());
        let mut index_skips = 0;
        let mut count = 0;
        while let Some(entry) = index_stream.next().await {
            // This can be a tight loop, so only check the elapsed time every
            // 100 times through, so that .elapsed() doesn't take significant
            // CPU time.
            count += 1;
            if count >= *MERGE_PROGRESS_CHECK_COUNT {
                count = 0;
                if timer.elapsed() >= *MERGE_PROGRESS_MESSAGE_INTERVAL {
                    // send free_list and current index phys to checkpointer
                    tx.send(MergeMessage::new_progress(next_index, free_list).await)
                        .await
                        .unwrap_or_else(|e| panic!("couldn't send: {}", e));
                    free_list = Vec::new();
                    timer = Instant::now();
                }
            }
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
            // index entry, which must be all Inserts (Removes,
            // RemoveThenInserts, and AtimeUpdates refer to existing Index
            // entries).
            while let Some((&pc_key, &PendingChange::Insert(pc_value))) =
                pending_changes_iter.peek()
            {
                if pc_key >= entry.key {
                    break;
                }
                // Add this new entry to the index
                self.add_to_index_or_evict(
                    IndexEntry {
                        key: pc_key,
                        value: pc_value,
                    },
                    next_index,
                    &mut free_list,
                );
                pending_changes_iter.next();
            }

            let next_pc_opt = pending_changes_iter.peek();
            match next_pc_opt {
                Some((&pc_key, &PendingChange::Remove())) => {
                    if pc_key == entry.key {
                        // Don't write this entry to the new generation.
                        // this pending change is consumed
                        pending_changes_iter.next();

                        // If None, we don't add this entry to the free list, as it'll be freed automatically via rebalance_fini().
                        if let Some(entry) = self.map_index_entry_to_rebalanced_location(entry) {
                            free_list.push(
                                entry
                                    .value
                                    .extent()
                                    .expect("PendingChange::Remove of ghost entry"),
                            );
                        }
                    } else {
                        // There shouldn't be a pending removal of an entry that doesn't exist in the index.
                        assert_gt!(pc_key, entry.key);
                        self.add_to_index_or_evict(entry, next_index, &mut free_list);
                    }
                }
                Some((&pc_key, &PendingChange::Insert(pc_value))) => {
                    // Most insertions are processed above.  There can't be an index
                    // entry with the same key unless we are replacing a ghost entry.
                    // Otherwise, it has to be removed first, resulting in a
                    // PendingChange::RemoveThenInsert.
                    if pc_key == entry.key {
                        // This key was re-inserted after falling out of the index.
                        // Replace the ghost index entry with the newly inserted entry.
                        assert!(
                            entry.value.location.is_none(),
                            "Insert of {:?} {:?} but {:?} is not ghost",
                            pc_key,
                            pc_value,
                            entry
                        );
                        self.add_to_index_or_evict(
                            IndexEntry {
                                key: pc_key,
                                value: pc_value,
                            },
                            next_index,
                            &mut free_list,
                        );
                        // this pending change is consumed
                        pending_changes_iter.next();
                    } else {
                        assert_gt!(pc_key, entry.key);
                        self.add_to_index_or_evict(entry, next_index, &mut free_list);
                    }
                }
                Some((&pc_key, &PendingChange::RemoveThenInsert(pc_value))) => {
                    if pc_key == entry.key {
                        // This key must have been removed (evicted) and then re-inserted.
                        // Add the pending change to the next generation instead of the current index's entry
                        assert_eq!(pc_value.size, entry.value.size);
                        self.add_to_index_or_evict(
                            IndexEntry {
                                key: pc_key,
                                value: pc_value,
                            },
                            next_index,
                            &mut free_list,
                        );

                        // this pending change is consumed
                        pending_changes_iter.next();
                    } else {
                        // We shouldn't have skipped any, because there has to be a corresponding Index entry
                        assert_gt!(pc_key, entry.key);
                        self.add_to_index_or_evict(entry, next_index, &mut free_list);
                    }
                }
                Some((&pc_key, &PendingChange::UpdateAtime(pc_value, _))) => {
                    if pc_key == entry.key {
                        // Add the pending entry to the next generation instead of the current index's entry
                        assert_eq!(pc_value.location, entry.value.location);
                        assert_eq!(pc_value.size, entry.value.size);
                        self.add_to_index_or_evict(
                            IndexEntry {
                                key: pc_key,
                                value: pc_value,
                            },
                            next_index,
                            &mut free_list,
                        );

                        // this pending change is consumed
                        pending_changes_iter.next();
                    } else {
                        // We shouldn't have skipped any, because there has to be a corresponding Index entry
                        assert_gt!(pc_key, entry.key);
                        self.add_to_index_or_evict(entry, next_index, &mut free_list);
                    }
                }
                None => {
                    // no more pending changes
                    self.add_to_index_or_evict(entry, next_index, &mut free_list);
                }
            }
        }
        let mut count = 0;
        while let Some((&pc_key, &PendingChange::Insert(pc_value))) = pending_changes_iter.peek() {
            count += 1;
            if count >= *MERGE_PROGRESS_CHECK_COUNT {
                count = 0;
                if timer.elapsed() >= *MERGE_PROGRESS_MESSAGE_INTERVAL {
                    // send free_list and current index phys to checkpointer
                    tx.send(MergeMessage::new_progress(next_index, free_list).await)
                        .await
                        .unwrap_or_else(|e| panic!("couldn't send: {}", e));
                    free_list = Vec::new();
                    timer = Instant::now();
                }
            }
            // Add this new entry to the index
            self.add_to_index_or_evict(
                IndexEntry {
                    key: pc_key,
                    value: pc_value,
                },
                next_index,
                &mut free_list,
            );
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

        // Send final checkpoint progress message with final free_list content
        tx.send(MergeMessage::new_progress(next_index, free_list).await)
            .await
            .unwrap_or_else(|e| panic!("couldn't send: {}", e));

        drop(old_index);
        next_index.flush().await;

        trace!("new histogram: {:#?}", next_index.atime_histogram);
        info!(
            "wrote next index with {} entries ({} MB) in {:.1}s ({:.1}MB/s)",
            next_index.log.len(),
            next_index.log.num_bytes() / 1024 / 1024,
            begin.elapsed().as_secs_f64(),
            (next_index.log.num_bytes() as f64 / 1024f64 / 1024f64) / begin.elapsed().as_secs_f64(),
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
            "took {}ms for rebalance to copy {} MB ({:.1}MB/s)",
            begin.elapsed().as_millis(),
            bytes_copied / 1024 / 1024,
            (bytes_copied as f64 / 1024f64 / 1024f64) / begin.elapsed().as_secs_f64(),
        );
    }
}

struct ZettaCacheState {
    block_access: Arc<BlockAccess>,
    primary: PrimaryPhys,
    guid: u64,
    primary_disk: DiskId,
    block_allocator: BlockAllocator,
    pending_changes: BTreeMap<IndexKey, PendingChange>,
    // Keep state associated with any on-going merge here
    merge: Option<Arc<MergeState>>,
    index_cache: LruCache<IndexKey, IndexValue>,
    // XXX Given that we have to lock the entire State to do anything, we might
    // get away with this being a Rc?  And the ExtentAllocator doesn't really
    // need the lock inside it.  But hopefully we split up the big State lock
    // and then this is useful.  Same goes for block_access.
    extent_allocator: Arc<ExtentAllocator>,
    atime_histogram: AtimeHistogramPhys, // includes pending_changes, including AtimeUpdate which is not logged
    size_histogram: SizeHistogramPhys,
    // XXX move this to its own file/struct with methods to load, etc?
    operation_log: BlockBasedLog<OperationLogEntry>,
    // When i/o completes, the value will be sent, and the entry can be removed
    // from the tree.  These are needed to prevent the ExtentAllocator from
    // overwriting them while i/o is in flight, and to ensure that writes
    // complete before we complete the next checkpoint.
    // XXX I don't think we do lookups here so these could be Vec's?
    outstanding_reads: BTreeMap<IndexValue, Arc<Semaphore>>,
    outstanding_writes: BTreeMap<IndexValue, Arc<Semaphore>>,

    atime: Atime,
    stats: Arc<CacheStats>,
}

pub struct LockedKey(LockedItem<IndexKey>);

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
            block_allocator: BlockAllocatorPhys::new(data_capacity),
            extent_allocator: ExtentAllocatorPhys::new(metadata_capacity),
            index: Default::default(),
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
        .write_all(DiskId(0), guid, block_access)
        .await;
    }

    pub async fn open(paths: Vec<&str>) -> ZettaCache {
        let block_access = Arc::new(BlockAccess::new(
            paths.iter().map(|path| Disk::new(path, false)).collect(),
            false,
        ));

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

        let index = ZettaCacheIndex::open(
            block_access.clone(),
            extent_allocator.clone(),
            checkpoint.index,
        )
        .await;

        let mut sysinfo = System::new();
        sysinfo.refresh_system();
        let system_memory = usize::from64(sysinfo.total_memory() * 1024);
        let index_cache_entries_bytes = (*INDEX_CACHE_ENTRIES_MEM_PCT * system_memory) / 100;
        let index_cache_entry_size = mem::size_of::<IndexKey>() + mem::size_of::<IndexValue>();
        let index_cache_cap =
            (((*INDEX_CACHE_ENTRIES_MEM_PCT) * system_memory) / 100) / index_cache_entry_size;
        info!(
            "index-cache capacity set to {} entries [{}% of {} - {} with entry size {}]",
            index_cache_cap,
            *INDEX_CACHE_ENTRIES_MEM_PCT,
            nice_p2size(system_memory as u64),
            nice_p2size(index_cache_entries_bytes as u64),
            nice_p2size(index_cache_entry_size as u64)
        );

        // XXX would be nice to periodically load the operation_log and verify
        // that our state's pending_changes & atime_histogram match it
        let mut atime_histogram = index.atime_histogram.clone();

        // We must be load the "old" operation log contained in the merge's progress, before we load the "current" operation
        // log. This is because the operations need to be loaded in the order in which they originally occurred.
        let old_pending_changes = match checkpoint.merge_progress.as_ref() {
            Some(progress) => {
                let old_operation_log = BlockBasedLog::open(
                    block_access.clone(),
                    extent_allocator.clone(),
                    progress.operation_log.clone(),
                );

                Some(Self::load_operation_log(&old_operation_log, &mut atime_histogram).await)
            }
            None => None,
        };

        let pending_changes = Self::load_operation_log(&operation_log, &mut atime_histogram).await;
        debug!("atime_histogram: {:#?}", atime_histogram);

        let stats = Arc::new(CacheStats::default());

        let mut state = ZettaCacheState {
            block_access: block_access.clone(),
            pending_changes,
            merge: None,
            index_cache: LruCache::new(index_cache_cap),
            atime_histogram,
            size_histogram: checkpoint.size_histogram,
            operation_log,
            primary,
            primary_disk,
            guid,
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

            let next_index = checkpoint
                .merge_progress
                .as_ref()
                .map(|phys| phys.index.clone());

            // Write out a new checkpoint (including superblocks on new disks)
            // now, rather than waiting a minute.  This way we minimize the
            // window between starting the agent with new disks and having the
            // on-disk state reflect those new disks being part of the pool.
            state.flush_checkpoint(&index, next_index).await;
        }

        let this = ZettaCache {
            index: Arc::new(tokio::sync::RwLock::new(index)),
            state: Arc::new(tokio::sync::Mutex::new(state)),
            outstanding_lookups: LockSet::new(),
            blocking_buffer_bytes_available: Arc::new(Semaphore::new(
                *CACHE_INSERT_BLOCKING_BUFFER_BYTES,
            )),
            nonblocking_buffer_bytes_available: Arc::new(Semaphore::new(
                *CACHE_INSERT_NONBLOCKING_BUFFER_BYTES,
            )),
            write_slots: Arc::new(Semaphore::new(
                block_access.disks().count() * *DISK_WRITE_MAX_QUEUE_DEPTH,
            )),
            block_access,
            stats,
            timebase: Instant::now(),
            cache_runtime_id: Uuid::new_v4(),
        };

        let (merge_rx, merge_index) = match checkpoint.merge_progress {
            Some(progress) => (
                Some(
                    this.state
                        .lock()
                        .await
                        .resume_merge_task(
                            this.index.clone(),
                            old_pending_changes.unwrap(),
                            progress.clone(),
                        )
                        .await,
                ),
                Some(progress.index),
            ),
            None => (None, None),
        };

        let my_cache = this.clone();
        tokio::spawn(async move {
            my_cache.checkpoint_task(merge_rx, merge_index).await;
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

        this
    }

    /// Load the provided operation log to produce a new pending changes map.
    /// Update the atime_histogram with the data from the pending changes.
    async fn load_operation_log(
        operation_log: &BlockBasedLog<OperationLogEntry>,
        atime_histogram: &mut AtimeHistogramPhys,
    ) -> BTreeMap<IndexKey, PendingChange> {
        let begin = Instant::now();
        let mut num_insert_entries: u64 = 0;
        let mut num_remove_entries: u64 = 0;
        let mut pending_changes = BTreeMap::new();
        operation_log
            .iter()
            .for_each(|entry| {
                match entry {
                    OperationLogEntry::Insert(key, value) => {
                        match pending_changes.entry(key) {
                            btree_map::Entry::Occupied(mut oe) => match oe.get() {
                                PendingChange::Remove() => {
                                    super_trace!("insert with existing removal; changing to RemoveThenInsert: {:?} {:?}", key, value);
                                    oe.insert(PendingChange::RemoveThenInsert(value));
                                }
                                pc  => {
                                    panic!(
                                        "Inserting {:?} {:?} into already existing entry {:?}",
                                        key,
                                        value,
                                        pc,
                                    );
                                }
                            },
                            btree_map::Entry::Vacant(ve) => {
                                super_trace!("insert {:?} {:?}", key, value);
                                ve.insert(PendingChange::Insert(value));
                            }
                        }
                        num_insert_entries += 1;
                        atime_histogram.insert(value);
                    }
                    OperationLogEntry::Remove(key, value) => {
                        match pending_changes.entry(key) {
                            btree_map::Entry::Occupied(mut oe) => match oe.get() {
                                PendingChange::Insert(value) => {
                                    super_trace!("remove with existing insert; clearing {:?} {:?}", key, value);
                                    oe.remove();
                                }
                                PendingChange::RemoveThenInsert(value) => {
                                    super_trace!("remove with existing removetheninsert; changing to remove: {:?} {:?}", key, value);
                                    oe.insert(PendingChange::Remove());
                                }
                                pc  => {
                                    panic!(
                                        "Removing {:?} from already existing entry {:?}",
                                        key,
                                        pc,
                                    );
                                }
                            },
                            btree_map::Entry::Vacant(ve) => {
                                super_trace!("remove {:?} {:?}", key, value);
                                ve.insert(PendingChange::Remove());
                            }
                        }
                        num_remove_entries += 1;
                        atime_histogram.remove(value);
                    }
                };
                future::ready(())
            })
            .await;
        info!(
            "loaded operation_log from {} inserts and {} removes into {} pending_changes in {}ms",
            num_insert_entries,
            num_remove_entries,
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
        mut merge_rx: Option<tokio::sync::mpsc::Receiver<MergeMessage>>,
        mut next_index: Option<ZettaCacheIndexPhys>,
    ) {
        let mut next_tick = tokio::time::Instant::now();
        loop {
            next_tick = std::cmp::max(
                tokio::time::Instant::now(),
                next_tick + *CHECKPOINT_INTERVAL,
            );
            // if there is no current merging state, check to see if a merge should be started
            if self.state.lock().await.merge.is_none() {
                assert!(merge_rx.is_none());
                assert!(next_index.is_none());
                merge_rx = self
                    .state
                    .lock()
                    .await
                    .try_start_merge_task(self.index.clone())
                    .await;
            }
            if let Some(rx) = &mut merge_rx {
                let mut msg_count = 0;
                let mut free_count = 0;
                // we have a channel to an active merge task, check it for messages
                loop {
                    let result = timeout_at(next_tick, rx.recv()).await;
                    match result {
                        // capture merge progress: the current next index phys and eviction requests
                        Ok(Some(MergeMessage::Progress(merge_checkpoint))) => {
                            msg_count += 1;
                            free_count += merge_checkpoint.free_list.len();
                            trace!(
                                "merge checkpoint with {} free requests",
                                merge_checkpoint.free_list.len()
                            );
                            next_index = Some(merge_checkpoint.new_index);
                            // free the extent ranges associated with the evicted blocks
                            // XXX - should check to see if the extent is still in the "coverage" area.
                            // it seems possible that the meta-data area could grow during the merge cycle.

                            for extent in merge_checkpoint.free_list {
                                super_trace!("eviction requested for {:?}", extent);
                                self.state.lock().await.block_allocator.free(extent);
                            }
                        }
                        // merge task complete, replace the current index with the new index
                        Ok(Some(MergeMessage::Complete(new_index))) => {
                            let mut index = self.index.write().await;

                            let mut state = self.state.lock().await;
                            state.rotate_index(&mut index, new_index).await;
                            state.block_allocator.rebalance_fini();

                            next_index = None;
                            merge_rx = None;
                            break;
                        }
                        Ok(None) => panic!("channel closed before Complete message received"),
                        Err(_) => break, // timed out
                    }
                }
                debug!(
                    "processed {} merge checkpoints with {} evictions requested",
                    msg_count, free_count,
                );
            }

            // flush out a new checkpoint every CHECKPOINT_INTERVAL to capture the current state
            sleep_until(next_tick).await;
            {
                let index = self.index.read().await;
                self.state
                    .lock()
                    .await
                    .flush_checkpoint(&index, next_index.clone())
                    .await;
            }
        }
    }

    pub async fn lookup(
        &self,
        guid: PoolGuid,
        block: BlockId,
        source: LookupSource,
    ) -> LookupResponse {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let key = IndexKey { guid, block };
        let locked_key = LockedKey(self.outstanding_lookups.lock(key).await);

        let response = if *CACHE_EVICT_EACH_N_LOOKUPS != 0
            && COUNTER.fetch_add(1, Ordering::Relaxed) == *CACHE_EVICT_EACH_N_LOOKUPS
        {
            COUNTER.store(0, Ordering::Relaxed);
            let evict_response = self.evict(locked_key).await;
            // bump stat after waiting so that it coincides with the lookup count stat
            self.stats.track_count(CacheMissForcedEviction);
            evict_response
        } else {
            let bytes = self
                .lookup_impl(&key, source, |state, value| {
                    if matches!(source, LookupSource::Read) {
                        state.size_histogram.lookup();
                    }
                    match value {
                        Some(value) => future::Either::Left(state.lookup(key, value, source)),
                        None => future::Either::Right(future::ready(None)),
                    }
                })
                .await;

            match bytes {
                Some(bytes) => {
                    self.stats.track_bytes(LookupBytes, bytes.len() as u64);
                    super_trace!("cache hit for {:?}", key);
                    LookupResponse::Present((bytes, locked_key))
                }
                None => LookupResponse::Absent(locked_key),
            }
        };

        match source {
            LookupSource::Write => self.stats.track_count(LookupForWrite),
            LookupSource::Read => self.stats.track_count(LookupForRead),
            LookupSource::Evict => {} // not possible for this code path
        }

        response
    }

    pub async fn evict(&self, locked_key: LockedKey) -> LookupResponse {
        let key = *locked_key.0.value();
        let response = self
            .lookup_impl(&key, LookupSource::Write, |state, value| {
                if let Some(value) = value {
                    state.evict(key, value);
                }
                future::ready(LookupResponse::Absent(locked_key))
            })
            .await;

        self.stats.track_count(Evictions);
        assert!(matches!(response, LookupResponse::Absent(_)));
        response
    }

    async fn lookup_impl<F, R, Fut>(&self, key: &IndexKey, source: LookupSource, f: F) -> R
    where
        F: FnOnce(&mut ZettaCacheState, Option<ValidIndexValue>) -> Fut,
        Fut: Future<Output = R>,
    {
        // Hold the index lock over the whole operation
        // so that the index can't change after we get the value from it.
        // Lock ordering requires that we lock the index before locking the state.
        let index = self.index.read().await;
        let fut_or_f = {
            // We don't want to hold the state lock while reading from disk so we
            // use lock_non_send() to ensure that we can't hold it across .await.
            let mut state = self.state.lock_non_send().await;
            match state.pending_changes.get(key).copied() {
                Some(pc) => {
                    match pc {
                        PendingChange::Insert(value)
                        | PendingChange::RemoveThenInsert(value)
                        | PendingChange::UpdateAtime(value, _) => {
                            let validated = state.validate(value);
                            // All entries in the pending changes should be valid
                            assert!(validated.is_some());
                            Either::Left(f(&mut state, validated))
                        }
                        PendingChange::Remove() => {
                            // Pending change says this has been removed
                            Either::Left(f(&mut state, None))
                        }
                    }
                }
                None => {
                    if let Some(ms) = &state.merge {
                        if let Some(pc) = ms.old_pending_changes.get(key).copied() {
                            match pc {
                                PendingChange::Insert(value)
                                | PendingChange::RemoveThenInsert(value)
                                | PendingChange::UpdateAtime(value, _) => {
                                    state.ghost_hit_check(value, source);
                                    let validated = state.validate(value);
                                    Either::Left(f(&mut state, validated))
                                }
                                PendingChange::Remove() => {
                                    // Pending change says this has been removed
                                    Either::Left(f(&mut state, None))
                                }
                            }
                        } else {
                            match state.index_cache.get(key) {
                                Some(&value) => {
                                    state.ghost_hit_check(value, source);
                                    let validated = state.validate(value);
                                    Either::Left(f(&mut state, validated))
                                }
                                None => Either::Right(f),
                            }
                        }
                    } else {
                        match state.index_cache.get(key) {
                            Some(&value) => {
                                state.ghost_hit_check(value, source);
                                let validated = state.validate(value);
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
                let result = fut.await;
                if matches!(source, LookupSource::Read) {
                    self.stats.track_count(CacheHitWithoutIndexRead);
                }
                return result;
            }
            Either::Right(f) => f,
        };

        super_trace!(
            "lookup has no pending_change and is absent from the index-cache; checking index for {:?}",
            key
        );

        // TODO -- is CacheMissWithoutIndexRead possible anymore? See DOSE-939
        let stat_counter;
        let fut = match index.log.lookup_by_key(key, |entry| entry.key).await {
            None => {
                // key not in index
                // XXX We don't really know if we read the index from disk. We
                // might have hit in the index chunk cache.  Same below.
                stat_counter = CacheMissAfterIndexRead;
                super_trace!("cache miss after reading index for {:?}", key);
                let mut state = self.state.lock_non_send().await;
                f(&mut state, None)
            }
            Some(entry) => {
                // Again, we don't want to hold the state lock while reading from disk so
                // we use lock_non_send() to ensure that we can't hold it across .await.
                let mut state = self.state.lock_non_send().await;
                let value = match &state.merge {
                    Some(ms) if entry.value.atime < ms.eviction_cutoff => {
                        // Block is being evicted, abort the read attempt
                        stat_counter = CacheMissAfterIndexRead;
                        super_trace!("cache miss after reading index, eviction cutoff {:?}", key);
                        None
                    }
                    Some(_) | None => {
                        stat_counter = CacheHitAfterIndexRead;
                        state.lookup_with_value_from_index(key, entry.value, source)
                    }
                };

                f(&mut state, value)
            }
        };
        let result = fut.await;

        // Update relevant stat after waiting
        if matches!(source, LookupSource::Read) {
            self.stats.track_count(stat_counter);
        }
        result
    }

    async fn reserve_buffer_space(
        &self,
        bytes: usize,
        source: InsertSource,
    ) -> Option<OwnedSemaphorePermit> {
        // The permit should be dropped when the write to disk completes.  It
        // serves to limit the number of insert()'s that we can buffer before
        // dropping (ignoring) insertion requests.
        let bytes32 = u32::try_from(bytes).unwrap();
        match source {
            InsertSource::Heal | InsertSource::SpeculativeRead | InsertSource::Write => match self
                .nonblocking_buffer_bytes_available
                .clone()
                .try_acquire_many_owned(bytes32)
            {
                Ok(permit) => {
                    self.stats.track_instantaneous(
                        NonblockingBufferBytesAvailable,
                        (*CACHE_INSERT_NONBLOCKING_BUFFER_BYTES
                            - self.nonblocking_buffer_bytes_available.available_permits())
                            as u64,
                    );
                    Some(permit)
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => None,
                Err(e) => panic!("unexpected error from try_acquire_many_owned: {:?}", e),
            },
            InsertSource::Read => {
                let permit = self
                    .blocking_buffer_bytes_available
                    .clone()
                    .acquire_many_owned(bytes32)
                    .await
                    .expect("error from acquire_many_owned");
                self.stats.track_instantaneous(
                    BlockingBufferBytesAvailable,
                    (*CACHE_INSERT_BLOCKING_BUFFER_BYTES
                        - self.blocking_buffer_bytes_available.available_permits())
                        as u64,
                );
                Some(permit)
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
            let fut = state.lock_non_send().await.insert(locked_key, bytes);
            fut.await;
            // We want to hold onto the insert_permit until the write completes
            // because it represents the memory that's required to buffer this
            // insertion, which isn't released until the io completes.
            // Similarly, the write_permit (roughly) represents the disks'
            // capacity to perform i/o.
            drop(insert_permit);
        });
    }

    pub async fn ingest_all(
        &self,
        guid: PoolGuid,
        blocks: &HashMap<BlockId, Bytes>,
        source: InsertSource,
    ) {
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
            futures.push(async move {
                let key = IndexKey { guid, block };
                let locked_key = LockedKey(cache.outstanding_lookups.lock(key).await);

                // We need to check for presence in the cache even for
                // InsertSource::Write, where we expect to be writing a "new"
                // BlockId that's never been written before, because if the
                // system crashed or the pool was rewound, a BlockId that was
                // already persisted to the cache may be reused.

                let present = cache
                    .lookup_impl(&key, LookupSource::Write, |_state, value| {
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
                    let fut = cache
                        .state
                        .lock_non_send()
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
            // For (hopefully) obvious reasons, we only need to do the eviction when the bytes contained in the cache differ
            // from the bytes contained in the object store. The bytes contained in the object store are always preferred
            // over the bytes contained in the cache; we assume the bytes passed were retrieved from the object store.
            if *cache_bytes != *object_bytes {
                self.stats.track_count(HealedBlocks);
                debug!("Healing cache: {:?}", locked_key.0.value());
                match self.evict(locked_key).await {
                    LookupResponse::Present((_, locked_key)) => {
                        panic!("evicted key is present! {:?}", locked_key.0.value());
                    }
                    LookupResponse::Absent(locked_key) => {
                        self.insert(locked_key, object_bytes, InsertSource::Heal)
                            .await
                    }
                }
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
    pub async fn open(paths: Vec<&str>) -> Result<ZCacheDBHandle> {
        let block_access = Arc::new(BlockAccess::new(
            paths.iter().map(|path| Disk::new(path, true)).collect(),
            true,
        ));

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
        println!("Superblock");
        println!("  Primary {:?}, GUID: {}", self.primary_disk, self.guid);
        println!();
        println!("Checkpoint Region");
        println!("  {:?}", self.primary.checkpoint_capacity);
        println!(
            "  checkpoint: {} used out of {} ({:.1}%, must be <50%)",
            nice_p2size(self.primary.checkpoint.size),
            nice_p2size(self.primary.checkpoint_capacity.size),
            self.primary.checkpoint.size as f64 * 100.0
                / self.primary.checkpoint_capacity.size as f64
        );
        println!();
        println!("Metadata Region");
        let mut total_used_bytes = 0;
        let mut total_allocated_bytes = 0;
        println!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "operation log",
            nice_p2size(self.checkpoint.operation_log.bytes()),
            nice_p2size(self.checkpoint.operation_log.capacity_bytes())
        );
        total_used_bytes += self.checkpoint.operation_log.bytes();
        total_allocated_bytes += self.checkpoint.operation_log.capacity_bytes();

        println!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "spacemap",
            nice_p2size(self.checkpoint.block_allocator.spacemap_bytes()),
            nice_p2size(self.checkpoint.block_allocator.spacemap_capacity_bytes())
        );
        total_used_bytes += self.checkpoint.block_allocator.spacemap_bytes();
        total_allocated_bytes += self.checkpoint.block_allocator.spacemap_capacity_bytes();

        println!(
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

        println!(
            "  {:>13} - {:>6} used out of {:>6} allocated",
            "index log",
            nice_p2size(self.checkpoint.index.log_bytes()),
            nice_p2size(self.checkpoint.index.log_capacity_bytes())
        );
        total_used_bytes += self.checkpoint.index.log_bytes();
        total_allocated_bytes += self.checkpoint.index.log_capacity_bytes();

        if let Some(progress) = self.checkpoint.merge_progress.clone() {
            println!(
                "  {:>13} - {:>6} used out of {:>6} allocated",
                "progress log",
                nice_p2size(progress.operation_log.bytes()),
                nice_p2size(progress.operation_log.capacity_bytes())
            );
            total_used_bytes += progress.operation_log.bytes();
            total_allocated_bytes += progress.operation_log.capacity_bytes();
            println!(
                "  {:>13} - {:>6} used out of {:>6} allocated",
                "progress index",
                nice_p2size(progress.index.log_bytes()),
                nice_p2size(progress.index.log_capacity_bytes())
            );
            total_used_bytes += progress.index.log_bytes();
            total_allocated_bytes += progress.index.log_capacity_bytes();
        }
        println!("  ----------------------");
        let metadata_region_size = self
            .checkpoint
            .extent_allocator
            .capacity
            .iter()
            .map(|extent| extent.size)
            .sum();
        println!(
            "  {:>13} - {} ({:.1}%) used, {} ({:.1}%) allocated out of {:>6} total",
            "total",
            nice_p2size(total_used_bytes),
            total_used_bytes as f64 * 100.0 / metadata_region_size as f64,
            nice_p2size(total_allocated_bytes),
            total_allocated_bytes as f64 * 100.0 / metadata_region_size as f64,
            nice_p2size(metadata_region_size)
        );
        println!();

        let balloc_size = self
            .checkpoint
            .block_allocator
            .capacity()
            .iter()
            .map(|extent| extent.size)
            .sum();
        println!("{:>6} User Data Region", nice_p2size(balloc_size));
    }

    pub async fn dump_structures(&self, opts: DumpStructuresOptions) {
        if opts.dump_defaults {
            println!("{:#?}", self.primary);
            println!("{:#?}", self.checkpoint);
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
                    println!("{:#?}", chunk);
                })
                .await;
        }

        if opts.dump_index_log_raw {
            self.checkpoint
                .index
                .iter_log_chunks(self.block_access.clone())
                .for_each(|chunk| async move {
                    println!("{:#?}", chunk);
                })
                .await;

            self.checkpoint
                .index
                .iter_log_summary(self.block_access.clone())
                .for_each(|chunk| async move {
                    println!("{:#?}", chunk);
                })
                .await
        }

        if opts.dump_rebalance_log_raw {
            if let Some(progress) = &self.checkpoint.merge_progress {
                if let Some(log) = progress.rebalance_log.as_ref() {
                    log.iter_chunks(self.block_access.clone())
                        .for_each(|chunk| async move {
                            println!("{:#?}", chunk);
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
}

pub struct ValidIndexValue(IndexValue);

impl ValidIndexValue {
    pub fn extent(&self) -> Extent {
        Extent {
            location: self.0.location.unwrap(),
            size: u64::from(self.0.size),
        }
    }
}

impl ZettaCacheState {
    /// Validates the passed in index value; returns the value back if it's still valid, or None.
    fn validate(&self, value: IndexValue) -> Option<ValidIndexValue> {
        let live_cutoff = match &self.merge {
            Some(ms) => ms.eviction_cutoff,
            None => self.atime_histogram.first_live(),
        };

        if value.atime < live_cutoff {
            None
        } else {
            assert!(value.location.is_some());
            Some(ValidIndexValue(value))
        }
    }

    fn ghost_hit_check(&mut self, value: IndexValue, source: LookupSource) {
        let (live_cutoff, ghost_cutoff) = match &self.merge {
            Some(ms) => (ms.eviction_cutoff, ms.ghost_cutoff),
            None => (
                self.atime_histogram.first_live(),
                self.atime_histogram.first(),
            ),
        };

        if value.atime >= ghost_cutoff
            && value.atime < live_cutoff
            && matches!(source, LookupSource::Read)
        {
            // This is a hit in the ghost range of the hit-by-size histogram
            let size = self.atime_histogram.size_at(value.atime);
            self.size_histogram.hit(size);
        }
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
            | Some(PendingChange::RemoveThenInsert(value_ref))
            | Some(PendingChange::UpdateAtime(value_ref, _)) => *value_ref,
            Some(PendingChange::Remove()) => return None,
            None => value_from_index,
        };
        self.ghost_hit_check(value, source);
        self.validate(value)
    }

    fn evict(&mut self, key: IndexKey, value: ValidIndexValue) {
        let value = value.0;
        self.remove_from_index(key, value);
    }

    fn lookup(
        &mut self,
        key: IndexKey,
        valid_value: ValidIndexValue,
        source: LookupSource,
    ) -> impl Future<Output = Option<AlignedBytes>> {
        let mut value = valid_value.0;
        trace!("cache hit: reading {:?} from {:?}", key, value);
        let original_atime = value.atime;
        if value.atime != self.atime {
            self.atime_histogram.remove(value);
            value.atime = self.atime;
            self.atime_histogram.insert(value);
        }

        // XXX looking up again.  But can't pass in both &mut self and &mut PendingChange
        let pc = self.pending_changes.get_mut(&key);
        match pc {
            Some(PendingChange::Insert(value_ref))
            | Some(PendingChange::RemoveThenInsert(value_ref))
            | Some(PendingChange::UpdateAtime(value_ref, _)) => {
                *value_ref = value;
            }
            Some(PendingChange::Remove()) => {
                panic!("invalid state")
            }
            None => {
                // only in Index, not pending_changes
                trace!(
                    "adding UpdateAtime to pending_changes {:?} {:?}, original atime {:?}",
                    key,
                    value,
                    original_atime
                );
                // XXX would be nice to have saved the btreemap::Entry so we
                // don't have to traverse the tree again.
                self.pending_changes
                    .insert(key, PendingChange::UpdateAtime(value, original_atime));
                self.update_pending_stats();
            }
        }
        if matches!(source, LookupSource::Read) {
            // Add an entry to the hit-by-size histogram
            let size = self.atime_histogram.size_at(original_atime);
            trace!(
                "cache size {} at atime {:?}, current atime_histogram size: {:?}",
                size,
                original_atime,
                self.atime_histogram.size_at(self.atime_histogram.first())
            );
            self.size_histogram.hit(size);
        }
        // If there's a write to this location in progress, we will need to wait for it to complete before reading.
        // Since we won't be able to remove the entry from outstanding_writes after we wait, we just get the semaphore.
        let write_sem_opt = self
            .outstanding_writes
            .get_mut(&value)
            .map(|arc| arc.clone());

        let sem = Arc::new(Semaphore::new(0));
        self.outstanding_reads.insert(value, sem.clone());
        let block_access = self.block_access.clone();

        async move {
            if let Some(write_sem) = write_sem_opt {
                trace!("{:?} at {:?}: waiting for outstanding write", key, value);
                let _permit = write_sem.acquire().await.unwrap();
            }

            let bytes = block_access
                .read_raw(valid_value.extent(), DiskIoType::ReadDataForLookup)
                .await;
            sem.add_permits(1);
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

    fn remove_from_index(&mut self, key: IndexKey, value: IndexValue) {
        let mut oplog_value = value;
        match self.pending_changes.get_mut(&key) {
            Some(PendingChange::Insert(value_ref)) => {
                // The operation_log has an Insert for this key, and the key
                // is not in the Index.  We don't need a
                // PendingChange::Removal since there's nothing to remove
                // from the index.
                assert_eq!(*value_ref, value);
                trace!("removing Insert from pending_changes {:?} {:?}", key, value);
                self.pending_changes.remove(&key);
            }
            Some(PendingChange::RemoveThenInsert(value_ref)) => {
                // The operation_log has a Remove, and then an Insert for
                // this key, so the key is in the Index.  We need a
                // PendingChange::Remove so that the Index entry won't be
                // found.
                assert_eq!(*value_ref, value);
                trace!(
                    "changing RemoveThenInsert to Remove in pending_changes {:?} {:?}",
                    key,
                    value,
                );
                self.pending_changes.insert(key, PendingChange::Remove());
            }
            Some(PendingChange::UpdateAtime(value_ref, index_atime)) => {
                // The atime for this block has been updated (in pending changes), but that
                // update is not preserved in the operation log (we don't log atime updates).
                // So we need to associate the "original" atime from the index with this remove.
                oplog_value.atime = *index_atime;

                // It's just an atime update, so the operation_log doesn't
                // have an Insert for this key, but the key is in the
                // Index.
                assert_eq!(*value_ref, value);
                trace!(
                    "changing UpdateAtime to Remove in pending_changes {:?} {:?}",
                    key,
                    value,
                );
                self.pending_changes.insert(key, PendingChange::Remove());
            }
            Some(PendingChange::Remove()) => {
                panic!("invalid state");
            }
            None => {
                // only in Index, not pending_changes
                trace!("adding Remove to pending_changes {:?}", key);
                self.pending_changes.insert(key, PendingChange::Remove());
            }
        }
        trace!("adding Remove to operation_log {:?}", key);
        self.atime_histogram.remove(value);
        self.operation_log
            .append(OperationLogEntry::Remove(key, oplog_value));
        self.update_pending_stats();
    }

    /// Insert this block to the cache, if space and performance parameters
    /// allow.  It may be a recent cache miss, or a recently-written block.
    /// Returns a Future to be executed after the state lock has been dropped.
    fn insert(&mut self, locked_key: LockedKey, bytes: AlignedBytes) -> impl Future {
        let buf_size = bytes.len();
        let location = match self.allocate_block(u32::try_from(bytes.len()).unwrap()) {
            Some(location) => location,
            None => return future::Either::Left(async {}),
        };

        // XXX if this is past the last block of the main index, we can write it
        // there (and location_dirty:false) instead of logging it

        let key = *locked_key.0.value();
        let value = IndexValue {
            atime: self.atime,
            location: Some(location),
            size: u32::try_from(buf_size).unwrap(),
        };

        // XXX we'd like to assert that this is not already in the index
        // (otherwise we would need to use a PendingChange::RemoveThenInsert).
        // However, this is not an async fn so we can't do the read here.  We
        // could spawn a new task, but currently reading the index requires the
        // big lock.

        let entry = self.pending_changes.entry(key);
        match entry {
            btree_map::Entry::Occupied(mut oe) => match oe.get() {
                PendingChange::Remove() => {
                    trace!(
                        "adding RemoveThenInsert to pending_changes {:?} {:?}",
                        key,
                        value
                    );
                    oe.insert(PendingChange::RemoveThenInsert(value));
                }
                pc => {
                    // Already in cache; ignore this insertion request?  Or panic?
                    todo!("key: {:#?}, value: {:#?}, pc: {:#?}", key, value, pc);
                }
            },
            btree_map::Entry::Vacant(ve) => {
                super_trace!("adding Insert to pending_changes {:?} {:?}", key, value);
                ve.insert(PendingChange::Insert(value));
                self.update_pending_stats();
            }
        }
        self.atime_histogram.insert(value);

        super_trace!("adding Insert to operation_log {:?} {:?}", key, value);
        self.operation_log
            .append(OperationLogEntry::Insert(key, value));

        let sem = Arc::new(Semaphore::new(0));
        self.outstanding_writes.insert(value, sem.clone());

        let block_access = self.block_access.clone();
        // Note: locked_key can be dropped before the i/o completes, since the
        // changes to the State have already been made.
        future::Either::Right(async move {
            block_access
                .write_raw(location, bytes, DiskIoType::WriteDataForInsert)
                .await;
            sem.add_permits(1);
        })
    }

    /// returns offset, or None if there's no space
    fn allocate_block(&mut self, size: u32) -> Option<DiskLocation> {
        self.block_allocator.allocate(size).map(|extent| {
            self.block_access.verify_aligned(extent.location.offset);
            extent.location
        })
    }

    /// Flush out the current set of pending index changes. This is a recovery point in case of
    /// a system crash between index rewrites.
    async fn flush_checkpoint(
        &mut self,
        index: &ZettaCacheIndex,
        next_index: Option<ZettaCacheIndexPhys>,
    ) {
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

        // Wait for all outstanding reads, so that if the ExtentAllocator needs
        // to overwrite some blocks, there aren't any outstanding i/os to that
        // region.
        // XXX It would be better to only wait for the reads that are in the
        // region that we're overwriting.  But it will be tricky to do the
        // waiting down in the ExtentAllocator.  If we get that working, we'll
        // still need to clean up the outstanding_reads entries that have
        // completed, at some point.  Even as-is, letting them accumulate for a
        // whole checkpoint might not be great.  It might be "cleaner" to
        // run every second and remove completed entries.  Or have the read task
        // lock the outstanding_reads and remove itself (which might perform
        // worse due to contention on the global lock).
        let begin = Instant::now();
        for sem in self.outstanding_reads.values_mut() {
            let _permit = sem.acquire().await.unwrap();
        }
        debug!(
            "waited for {} outstanding_reads in {}ms",
            self.outstanding_reads.len(),
            begin.elapsed().as_millis()
        );
        self.outstanding_reads.clear();

        // Wait for all outstanding writes, for the same reason as reads, and
        // also so that if we crash, the blocks referenced by the
        // index/operation_log will actually have the correct contents.
        let begin = Instant::now();
        for sem in self.outstanding_writes.values_mut() {
            let _permit = sem.acquire().await.unwrap();
        }
        debug!(
            "waited for {} outstanding_writes in {}ms",
            self.outstanding_writes.len(),
            begin.elapsed().as_millis()
        );
        self.outstanding_writes.clear();

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
            "operation log: flushed {} entries to {} KB in {}ms",
            operation_log_len,
            operation_log_bytes / 1024,
            begin.elapsed().as_millis()
        );

        // Note that it is possible to have a merge in progress with no next_index available.
        // This can happen if we have not yet received any progress messages from the merge task.
        // In this case we just store an empty "in progress" index in the checkpoint.
        let merge_progress_phys = self.merge.as_ref().map(|ms| MergeProgressPhys {
            rebalance_log: ms
                .rebalance
                .as_ref()
                .map(|rebalance| rebalance.log_phys.clone()),
            operation_log: ms.old_operation_log_phys.clone(),
            index: next_index.unwrap_or_default(),
        });

        let checkpoint = ZettaCheckpointPhys {
            generation: self.primary.checkpoint_id.next(),
            extent_allocator: self.extent_allocator.get_phys(),
            index: index.get_phys(),
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
                warn!("possible leak of metadata space: {}MB available according to checkpoint but not in memory",
                    (checkpoint_bytes - allocator_bytes) / 1024 / 1024);
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
                self.primary.checkpoint.location.disk,
                self.primary.checkpoint.location.offset + self.primary.checkpoint.size,
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
            checkpoint_extent.location.offset = self.primary.checkpoint_capacity.location.offset;
            assert!(self
                .primary
                .checkpoint_capacity
                .contains(&checkpoint_extent));
            assert_le!(
                checkpoint_extent.location.offset + checkpoint_extent.size,
                self.primary.checkpoint.location.offset
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
            "completed {:?} in {}ms; flushed {} operations ({}KB) to log",
            self.primary.checkpoint_id,
            begin_checkpoint.elapsed().as_millis(),
            operation_log_len,
            operation_log_bytes / 1024,
        );
    }

    fn spawn_merge_task(
        &self,
        merge: Arc<MergeState>,
        old_index: Arc<tokio::sync::RwLock<ZettaCacheIndex>>,
        mut next_index: ZettaCacheIndex,
    ) -> tokio::sync::mpsc::Receiver<MergeMessage> {
        // The checkpoint task will be constantly reading from the channel, so we don't really need
        // much of a buffer here. We use 100 because we might accumulate some messages while actually
        // flushing out the checkpoint.
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        let block_access = self.block_access.clone();

        tokio::spawn(async move {
            merge
                .merge_task(tx.clone(), old_index, &mut next_index, &block_access)
                .await;

            // We drop this before sending the Complete message, so that rotate_index() can unwrap the Arc.
            drop(merge);

            // send the now complete next_index as the final message
            tx.send(MergeMessage::Complete(next_index))
                .await
                .unwrap_or_else(|e| panic!("couldn't send: {}", e));

            trace!("sent final checkpoint message");
        });

        rx
    }

    /// Restart a merge task from the saved checkpoint state
    async fn resume_merge_task(
        &mut self,
        old_index: Arc<tokio::sync::RwLock<ZettaCacheIndex>>,
        old_pending_changes: BTreeMap<IndexKey, PendingChange>,
        progress: MergeProgressPhys,
    ) -> tokio::sync::mpsc::Receiver<MergeMessage> {
        let next_index = ZettaCacheIndex::open(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            progress.index,
        )
        .await;
        info!(
            "restarting merge at {:?} with eviction atime {:?}",
            next_index.last_key,
            next_index.first_atime(),
        );

        let rebalance = match progress.rebalance_log {
            None => None,
            Some(log_phys) => {
                let map: BTreeMap<Extent, Option<DiskLocation>> = log_phys
                    .iter_entries(self.block_access.clone())
                    .map(|entry| (entry.old, entry.new))
                    .collect()
                    .await;

                Some(RebalanceState { log_phys, map })
            }
        };

        let merge = Arc::new(MergeState {
            old_operation_log_phys: progress.operation_log.clone(),
            ghost_cutoff: next_index.first_atime(),
            eviction_cutoff: next_index.first_live_atime(),
            old_pending_changes,
            rebalance,
            stats: self.stats.clone(),
        });
        self.merge = Some(merge.clone());

        self.spawn_merge_task(merge, old_index, next_index)
    }

    /// Start a new merge task if there are enough pending changes
    async fn try_start_merge_task(
        &mut self,
        old_index: Arc<tokio::sync::RwLock<ZettaCacheIndex>>,
    ) -> Option<tokio::sync::mpsc::Receiver<MergeMessage>> {
        if self.pending_changes.len() < *MAX_PENDING_CHANGES
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
        let ghost_atime = self
            .atime_histogram
            .atime_for_ghost_target(target_reduction / 100 * *GHOST_CACHE_SIZE_PCT);

        let old_operation_log_phys = self.operation_log.flush().await;

        // Create an empty operation log that is consistent with the empty pending state.
        // Note that we don't want to just clear the existing operation log, since we are
        // still preserving that in the merging state.
        self.operation_log = BlockBasedLog::open(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            Default::default(),
        );
        let next_index = ZettaCacheIndex::open(
            self.block_access.clone(),
            self.extent_allocator.clone(),
            ZettaCacheIndexPhys::new(ghost_atime, eviction_atime),
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
                    log.append(RebalanceLogEntry { old, new });
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

        Some(self.spawn_merge_task(merge, old_index, next_index))
    }

    /// Switch to the new index returned from the merge task and clear the merging state.
    /// Called with the old index write-locked.
    async fn rotate_index(&mut self, index: &mut ZettaCacheIndex, next_index: ZettaCacheIndex) {
        let mut merge = Arc::try_unwrap(self.merge.take().unwrap())
            .expect("unable to unwrap merge state during index rotation");

        // Free up the extents that have been allocated for the merge pending state
        merge.old_operation_log_phys.clear(&self.extent_allocator);

        if let Some(rebalance) = &merge.rebalance {
            let begin = Instant::now();

            let mut evicted_keys = Vec::new();
            for (key, pc) in self.pending_changes.iter_mut() {
                match pc {
                    PendingChange::UpdateAtime(value, _) => {
                        // If a lookup occurs on a block that is being moved as part of rebalancing, the lookup will
                        // return the "old" location of the block (which is valid while we are merging) and will be
                        // stored in an UpdateAtime record in pending_changes. Now that the merge is complete, we need
                        // to either: 1) remap these old locations to their "new" rebalanced locations, or 2) remove
                        // the UpdateAtime due to rebalancing having had to evict the entry from the cache (i.e. due
                        // to an allocation failure when attempting to allocate the new disk location).
                        match rebalance.remap(value.extent().unwrap()) {
                            Some(location) => value.location = Some(location),
                            None => evicted_keys.push(*key),
                        }
                    }
                    PendingChange::Insert(value) | PendingChange::RemoveThenInsert(value) => {
                        // Inserts in the "pending changes" list will never be moved as part of a rebalance.
                        let extent = value.extent().unwrap();
                        assert_eq!(rebalance.remap(extent).unwrap(), extent.location);
                    }
                    PendingChange::Remove() => {
                        // Nothing to do for removes.
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

            let begin = Instant::now();

            // Similar to the pending changes above, we also need to update the index cache, so that any values contained
            // by the cache will properly reflect the state of the cache post-rebalancing; otherwise, index cache hits could
            // refer to disk locations that have been freed or entries that have been evicted.
            let mut evicted_keys = Vec::new();
            for (key, value) in self.index_cache.iter_mut() {
                match rebalance.remap(value.extent().unwrap()) {
                    Some(location) => value.location = Some(location),
                    None => evicted_keys.push(*key),
                }
            }

            for key in evicted_keys.iter() {
                self.index_cache.pop(key);
            }

            debug!(
                "took {}ms to remap index cache with {} entries",
                begin.elapsed().as_millis(),
                self.index_cache.len()
            );
        }

        // Move the "start" of the zettacache state histogram to reflect the new index
        self.atime_histogram.reset_first(merge.ghost_cutoff);
        trace!(
            "reset incore histogram start to {:?}",
            self.atime_histogram.first()
        );
        self.atime_histogram.reset_first_live(merge.eviction_cutoff);

        let begin = Instant::now();

        // Populate index_cache with old_pending_changes
        for (key, pc) in &merge.old_pending_changes {
            match pc {
                PendingChange::Insert(mut value)
                | PendingChange::UpdateAtime(mut value, _)
                | PendingChange::RemoveThenInsert(mut value) => {
                    // For the "old pending changes" list, we need to not only do the remap for atime updates, but also
                    // for inserts. This is because an insert could have occurred just prior to the merge starting, and
                    // then the location for that new insert may have been rebalanced via the merge. In this case, we need
                    // to ensure index cache is populated correctly with the new location(s).
                    match self.validate(value) {
                        Some(_) => {
                            let remapped = match merge.rebalance.as_ref() {
                                Some(rebalance) => {
                                    rebalance.remap(value.extent().unwrap()).map(|location| {
                                        value.location = Some(location);
                                        value
                                    })
                                }
                                None => Some(value),
                            };

                            match remapped {
                                Some(value) => {
                                    self.index_cache.put(*key, value);
                                }
                                None => {
                                    self.index_cache.pop(key);
                                }
                            }
                        }
                        None => continue,
                    }
                }
                PendingChange::Remove() => {
                    // LruCache.pop() doesn't blow up if the key is not part of
                    // the cache - it just returns None. Thus it is safe to use
                    // here unconditionally.
                    self.index_cache.pop(key);
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

        // Free up the space used by the old index and rotate in the new index
        index.clear();
        *index = next_index;

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
