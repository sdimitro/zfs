use std::cmp::Ordering;
use std::fmt::Debug;
use std::mem;
use std::ops::Bound::Excluded;
use std::ops::Bound::Unbounded;
use std::sync::Arc;
use std::time::Instant;

use conv::ConvUtil;
use futures::StreamExt;
use log::*;
use more_asserts::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use util::measure;
use util::nice_p2size;
use util::super_trace;
use util::tunable;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::CacheStats;
use util::zettacache_stats::DiskIoType;

use super::remap::RemapLogEntry;
use super::remap::RemapState;
use super::OperationLogEntry;
use super::PendingChange;
use super::PendingChanges;
use super::UpdateAtime;
use crate::atime_histogram::AtimeHistogramPhys;
use crate::base_types::Atime;
use crate::base_types::Extent;
use crate::block_access::BlockAccess;
use crate::block_access::DISK_READ_MAX_QUEUE_DEPTH;
use crate::block_based_log::BlockBasedLogPhys;
use crate::index::IndexEntry;
use crate::index::IndexFlushDelta;
use crate::index::IndexKey;
use crate::index::IndexRun;
use crate::index::IndexRunPhys;
use crate::index::IndexValue;
use crate::slab_allocator::SlabAllocatorBuilder;

tunable! {
    // Limit this to half the read queue depth (per disk) so that we don't crowd
    // out normal reads too much.  Note that since writes aggregate, they
    // typically won't be the io bottleneck.
    static ref CACHE_REBALANCE_CONCURRENCY_LIMIT: usize = *DISK_READ_MAX_QUEUE_DEPTH / 2;

    static ref MERGE_PROGRESS_CHUNK: usize = 1_000_000;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MergeProgressPhys {
    pub(super) rebalance_log: Option<BlockBasedLogPhys<RemapLogEntry>>,
    pub(super) operation_log: BlockBasedLogPhys<OperationLogEntry>,
    pub(super) new_index: IndexRunPhys,
}

impl MergeProgressPhys {
    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.operation_log.claim(builder);
        self.new_index.claim(builder);
        if let Some(remap_log) = self.rebalance_log.as_ref() {
            remap_log.claim(builder);
        }
    }
}

#[derive(Debug)]
struct IndexMessage {
    last_key: IndexKey,
    entries: Vec<IndexEntry>,
    entries_atimes: AtimeHistogramPhys,
    frees: Vec<Extent>,
    cache_updates: Vec<IndexEntry>,
    obsoleted_atimes: AtimeHistogramPhys,
}

#[derive(Debug)]
pub(super) struct MergeProgress {
    pub(super) new_index: IndexRunPhys,
    pub(super) obsoleted: AtimeHistogramPhys,
    pub(super) index_delta: IndexFlushDelta,
    pub(super) frees: Vec<Extent>,
    pub(super) cache_updates: Vec<IndexEntry>,
}

#[allow(clippy::large_enum_variant)]
pub(super) enum MergeMessage {
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

/// The merge task sends incremental progress messages to the checkpoint task so that the
/// progress can be persisted (and restarted if the agent dies) and also so that space from
/// evicted blocks can become available without needing to wait for merge completion. Buffers for
/// accumulated work are pre-allocated to avoid the cost of growing those buffers during the
/// merge.
struct Progress {
    chunk_len: usize,
    tx: mpsc::Sender<IndexMessage>,
    last_key: Option<IndexKey>,
    entries: Vec<IndexEntry>,
    entries_atimes: AtimeHistogramPhys,
    frees: Vec<Extent>,
    // This contains a list of entries that will be used to update the index cache. These may
    // originate from new updates (i.e. from the pending changes list), or from disk location
    // changes (i.e. from a block allocator rebalance operation).
    cache_updates: Vec<IndexEntry>,
    obsoleted: AtimeHistogramPhys,
    timer: Instant,
}

enum IngestSource {
    Index,
    PendingChange,
}

impl Progress {
    fn new(
        tx: mpsc::Sender<IndexMessage>,
        first_ghost: Atime,
        first_live: Atime,
        histogram_len: usize,
    ) -> Self {
        let chunk_len = *MERGE_PROGRESS_CHUNK;
        Self {
            chunk_len,
            tx,
            last_key: None,
            entries: Vec::with_capacity(chunk_len),
            entries_atimes: AtimeHistogramPhys::with_capacity(
                first_ghost,
                first_live,
                histogram_len,
            ),
            frees: Vec::with_capacity(chunk_len),
            cache_updates: Vec::with_capacity(chunk_len),
            obsoleted: AtimeHistogramPhys::with_capacity(first_ghost, first_live, histogram_len),
            timer: Instant::now(),
        }
    }

    /// As entries from the old index are processed (possibly added to the new index), they are
    /// now "obsolete" in the old index, so need to be removed from the atime histogram.
    fn obsolete(&mut self, entry: IndexEntry) {
        self.obsoleted.insert_unchecked(entry.value);
    }

    /// When an old index entry already exists for a newly inserted key, the new entry will
    /// replace the old, so "evict" the old entry: if the entry is a ghost, then there is nothing
    /// to do, otherwise, add the entry to the free list.
    fn evict(&mut self, state: &MergeState, entry: IndexEntry) {
        if let Some(extent) = entry.value.extent() {
            match &state.remap {
                Some(remap) => {
                    // If remap() is None, the data was evicted by the rebalance, so there's
                    // nothing to free here.
                    if let Some(location) = remap.remap(extent) {
                        self.frees.push(Extent {
                            location,
                            size: extent.size,
                        });
                    }
                }
                None => self.frees.push(extent),
            }
        }
    }

    /// Like `ingest()`.  Returns true if `report().await` is needed.
    fn ingest_pc(&mut self, state: &MergeState, key: IndexKey, value: IndexValue) -> bool {
        self.ingest(
            state,
            IndexEntry::new(key, value),
            IngestSource::PendingChange,
        )
    }

    /// The provided index entry is either:
    /// 1. Added to the list of entries to be part of the new index, or
    /// 2. Added to the list of entries to be evicted from the cache, or
    /// 3. Dropped because it is an already evicted entry that is no longer being tracked.
    /// Returns true if report() is needed.  Note that we don't want to do the `report.await()`
    /// here because that would require instantiating a Future in the common case where we don't
    /// need to report(), which impacts performance because this is called very frequently.
    fn ingest(&mut self, state: &MergeState, mut entry: IndexEntry, source: IngestSource) -> bool {
        if let Some(extent) = entry.value.extent() {
            if let Some(remap) = &state.remap {
                let remapped_location = remap.remap(extent);
                if entry.value.location() != remapped_location {
                    // The data for this entry has been moved due to a cache remap
                    // operation. Update the entry using the new location for the data.
                    // Note: if remap was unable to move the data (evicting the entry
                    // instead) the new location will be None.
                    entry.value.set_location(remapped_location);
                    self.cache_updates.push(entry);
                }
            }
        }
        // We use `.0` so that the primitive u32 comparison is used, which the compiler can
        // better optimize compared to calling `<Atime as PartialOrd>::ge()`.
        if entry.value.atime().0 >= state.eviction_cutoff.0 {
            // If this entry was evicted during remap, don't put it in the new index
            if entry.value.location().is_some() {
                self.entries_atimes.insert_unchecked(entry.value);
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
                self.entries_atimes.insert_unchecked(entry.value);
                self.entries.push(entry);
            }
        }
        self.last_key = Some(entry.key);

        self.entries.len() >= self.chunk_len
            || self.frees.len() >= self.chunk_len
            || self.cache_updates.len() >= self.chunk_len
    }

    /// Send a message to the next_index_task, with the current set of index entries to add and
    /// the current set of freed entries. Note: if we don't have a "last_key" then there is
    /// nothing to send.
    async fn report(&mut self) {
        if let Some(last_key) = self.last_key {
            measure!("Progress::report() tx.send(IndexMessage)")
                .fut_timed(self.tx.send(IndexMessage {
                    last_key,
                    entries: mem::replace(&mut self.entries, Vec::with_capacity(self.chunk_len)),
                    entries_atimes: self.entries_atimes.take(),
                    frees: mem::replace(&mut self.frees, Vec::with_capacity(self.chunk_len)),
                    cache_updates: mem::replace(
                        &mut self.cache_updates,
                        Vec::with_capacity(self.chunk_len),
                    ),
                    obsoleted_atimes: self.obsoleted.take(),
                }))
                .await
                .unwrap_or_else(|e| panic!("couldn't send: {e}"));
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

#[derive(Debug)]
pub(super) struct MergeState {
    pub(super) remap: Option<RemapState>,
    pub(super) old_pending_changes: PendingChanges,
    pub(super) old_operation_log_phys: BlockBasedLogPhys<OperationLogEntry>,
    pub(super) eviction_cutoff: Atime, // lowest atime of live entries to keep in the new index
    pub(super) ghost_cutoff: Atime,    // lowest atime of ghost entries to keep in the new index
    pub(super) last_atime: Atime,      // highest atime that could be in the old or new index
    pub(super) stats: Arc<CacheStats>,
}

impl MergeState {
    /// This task offloads the task of writing out the next index from the merge task.  This
    /// allows the merge to proceed in parallel with the writes to disk.  Relatively large chunks
    /// of the new index are provided to make the IO as efficient as possible.
    async fn next_index_task(
        &self,
        mut merge_rx: mpsc::Receiver<IndexMessage>,
        checkpoint_tx: mpsc::Sender<MergeMessage>,
        next_index: &mut IndexRun,
    ) {
        let begin = Instant::now();
        while let Some(message) = merge_rx.recv().await {
            next_index.append(message.entries, &message.entries_atimes);
            // The "last key" from the appended entries may not be the last key we actually
            // processed in the merge (e.g, we may have evicted some entries later)
            next_index.update_last_key(message.last_key);
            checkpoint_tx
                .send(
                    MergeMessage::new_progress(
                        next_index,
                        message.frees,
                        message.cache_updates,
                        message.obsoleted_atimes,
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
        // We don't currently support concurrent free()'s while the remap is in-progress.
        // Thus, we need to do the remap first, prior to moving forward with the merge.
        self.remap(block_access).await;

        let begin = Instant::now();

        debug!("using {start_key:?} as start key for merge");
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
                self.last_atime - old_index.first_ghost_atime() + 1,
            );
        }
        let mut pending_changes_iter = self
            .old_pending_changes
            .range((start_key.map_or(Unbounded, Excluded), Unbounded))
            .peekable();

        while let Some(chunk) = measure!("MergeState::merge_task index_stream.next()")
            .fut_timed(index_stream.next())
            .await
        {
            let mut entries = chunk.entries();
            // If the next index is already "started", advance the old index to the start point.
            // The index_stream excludes the trimmed chunks, so this only happens within the
            // first chunk.  This could be done using `binary_search_by_key()`, but in the common
            // case (not the first chunk), this is faster because only a single check is needed.
            if let Some(start_key) = start_key {
                while !entries.is_empty() && entries[0].key <= start_key {
                    super_trace!("skipping index entry: {:?}", entries[0].key);
                    entries = &entries[1..];
                }
            }
            loop {
                // Process run of entries that do not involve pending changes.  This is the most
                // common and performance-critical path.
                let contiguous = match pending_changes_iter.peek() {
                    Some((&pc_key, _)) => {
                        match entries.binary_search_by_key(&pc_key, |entry| entry.key) {
                            Ok(index) | Err(index) => index,
                        }
                    }
                    None => entries.len(),
                };
                let (contiguous, remainder) = entries.split_at(contiguous);
                entries = remainder;
                for &entry in contiguous {
                    progress.obsolete(entry);
                    if progress.ingest(self, entry, IngestSource::Index) {
                        progress.report().await;
                    }
                }

                let entry = match entries.first() {
                    Some(&entry) => entry,
                    None => break,
                };

                while let Some((&pc_key, &pc)) = pending_changes_iter.peek() {
                    match pc_key.cmp(&entry.key) {
                        Ordering::Less => {
                            // Add this new entry to the index.  It must be an Insert, because an
                            // UpdateAtime applies to an existing entry.
                            if let PendingChange::Insert(pc_value) = pc {
                                if progress.ingest_pc(self, pc_key, pc_value) {
                                    progress.report().await;
                                }
                            } else {
                                panic!(
                                    "{pc_key:?} {pc:?} has no corresponding entry in the index run"
                                );
                            }
                            pending_changes_iter.next();
                        }
                        Ordering::Equal => {
                            // Note, obsolete() needs to be called before ingest_pc(), because we
                            // need to count its obsolescence before reporting.
                            progress.obsolete(entry);
                            match pc {
                                PendingChange::Insert(pc_value) => {
                                    // We are replacing an entry. This may be a ghost entry being
                                    // re-cached or a heal() of a bad entry.
                                    if entry.value.location().is_some() {
                                        debug!("Insert of {pc_value:?} replaces {entry:?}");
                                    }
                                    progress.evict(self, entry);
                                    if progress.ingest_pc(self, pc_key, pc_value) {
                                        progress.report().await;
                                    }
                                }
                                PendingChange::UpdateAtime(UpdateAtime(pc_value, _)) => {
                                    // Replace this entry with the pending change entry that
                                    // updates the atime.
                                    assert_eq!(pc_value.extent(), entry.value.extent());
                                    assert_ge!(pc_value.atime(), entry.value.atime());
                                    if progress.ingest_pc(self, pc_key, pc_value) {
                                        progress.report().await;
                                    }
                                }
                            }
                            // Both the pending change and the index run entry are consumed.
                            pending_changes_iter.next();
                            entries = &entries[1..];
                            break;
                        }
                        Ordering::Greater => break, // process entry next
                    }
                }
            }
        }
        while let Some((&pc_key, &PendingChange::Insert(pc_value))) = pending_changes_iter.peek() {
            // Add this new entry to the index
            if progress.ingest_pc(self, pc_key, pc_value) {
                progress.report().await;
            }
            // Consume pending change.  We don't do that in the `while let` because we want to
            // leave any unmatched items in the iterator so that we can print them out when
            // failing below.
            pending_changes_iter.next();
        }
        // Other pending changes refer to existing index entries and therefore should have been
        // processed above
        assert!(
            pending_changes_iter.peek().is_none(),
            "next={:?}",
            pending_changes_iter.peek().unwrap()
        );

        // Send final progress message with final list content
        progress.report().await;

        info!(
            "merge task completed in {:.1}s",
            begin.elapsed().as_secs_f64(),
        );
    }

    async fn remap(&self, block_access: &BlockAccess) {
        let map = match self.remap.as_ref() {
            None => return,
            Some(remap) => &remap.map,
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

        let mut copied_extents = 0;
        let mut copied_bytes = 0;
        let mut evicted_extents = 0;
        let mut evicted_bytes = 0;
        for (old, maybe_new) in map.iter() {
            match maybe_new {
                Some(_) => {
                    copied_extents += 1;
                    copied_bytes += old.size;
                }
                None => {
                    evicted_extents += 1;
                    evicted_bytes += old.size;
                }
            }
        }

        info!(
            "took {}ms to rebalance; copied {} ({} entries) ({}/s), evicted {} ({} entries)",
            begin.elapsed().as_millis(),
            nice_p2size(copied_bytes),
            copied_extents,
            nice_p2size(
                (copied_bytes as f64 / begin.elapsed().as_secs_f64())
                    .approx_as::<u64>()
                    .unwrap()
            ),
            nice_p2size(evicted_bytes),
            evicted_extents,
        );
    }

    pub(super) fn spawn_tasks(
        self: Arc<MergeState>,
        block_access: Arc<BlockAccess>,
        old_index: Arc<tokio::sync::RwLock<IndexRun>>,
        mut next_index: IndexRun,
    ) -> mpsc::Receiver<MergeMessage> {
        // The checkpoint task will be constantly reading from the channel, so we don't really
        // need much of a buffer here. We use 100 because we might accumulate some messages while
        // actually flushing out the checkpoint.
        let (index_tx, checkpoint_rx) = mpsc::channel(100);
        let (merge_tx, index_rx) = mpsc::channel(100);

        let start_key = next_index.last_key();

        let spawn_merge = self.clone();
        measure!("MergeState::merge_task()").spawn(async move {
            spawn_merge
                .merge_task(merge_tx, old_index, start_key, &block_access)
                .await;
        });

        measure!("MergeState::next_index_task()").spawn(async move {
            self.next_index_task(index_rx, index_tx.clone(), &mut next_index)
                .await;

            // We drop this before sending the Complete message, so that rotate_index() can
            // unwrap the Arc.
            drop(self);
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
}
