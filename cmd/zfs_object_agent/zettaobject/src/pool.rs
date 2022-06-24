use std::borrow::Borrow;
use std::cmp::max;
use std::cmp::min;
use std::collections::hash_map;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Display;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use bytes::Bytes;
use bytesize::ByteSize;
use derivative::Derivative;
use futures::future;
use futures::future::join3;
use futures::future::Future;
use futures::stream;
use futures::stream::*;
use futures::FutureExt;
use lazy_static::lazy_static;
use log::*;
use more_asserts::*;
use nvpair::NvList;
use serde::Deserialize;
use serde::Serialize;
use stream_reduce::Reduce;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use util::async_cache::GetMethod;
use util::maybe_die_with;
use util::measure;
use util::serde::from_json_slice;
use util::super_trace;
use util::tunable;
use util::tunable::Percent;
use util::unordered::Unordered;
use util::watch_once;
use util::with_alloctag;
use util::AlignedBytes;
use uuid::Uuid;
use zettacache::base_types::*;
use zettacache::InsertSource;
use zettacache::LookupResponse;
use zettacache::ZettaCache;

use crate::access_stats::ObjectAccessOpType;
use crate::base_types::*;
use crate::data_object::DataObject;
use crate::features;
use crate::features::FeatureError;
use crate::features::FeatureFlag;
use crate::heartbeat;
use crate::heartbeat::HeartbeatGuard;
use crate::heartbeat::HeartbeatPhys;
use crate::heartbeat::HEARTBEAT_INTERVAL;
use crate::heartbeat::LEASE_DURATION;
use crate::object_access::OAError;
use crate::object_access::ObjectAccess;
use crate::object_access::PutError;
use crate::object_based_log::*;
use crate::object_block_map::ObjectBlockMap;
use crate::object_block_map::StorageObjectLogEntry;
use crate::object_deleter::ObjectDeleter;
use crate::object_deleter::ObjectDeleterPhys;
use crate::pool_destroy;
use crate::pool_destroy::PoolDestroyingPhys;

tunable! {
    // start freeing when the pending frees are this % of the entire pool
    static ref FREE_HIGHWATER_PCT: Percent = Percent::new(10.0);
    // stop freeing when the pending frees are this % of the free log
    static ref FREE_LOWWATER_PCT: Percent = Percent::new(40.0);
    // don't bother freeing unless there are at least this number of free blocks
    static ref FREE_MIN_BLOCKS: u64 = 1000;
    static ref TARGET_OBJECT_SIZE: ByteSize = ByteSize::mib(2);

    // Split a reclaim free log when it exceeds this many entries.  We picked 10 million to
    // keep the memory size for loading pending frees and object sizes logs at about 1/2 GB.
    //
    // If this value is smaller than the number of blocks that could be freed in one object
    // group (1000 objects), then we may end up trying to repeatedly split a log that contains
    // blocks of only a single object group, because we'll send all the records to a single
    // "side" of the split. Given object size=2MB, group size=1000 objects, and min block
    // size=512b, the maximum blocks (and thus entries) in one object group is 4 million.
    // Therefore this setting should be >4M.
    static ref RECLAIM_LOG_ENTRIES_LIMIT: u64 = 10_000_000;

    // When reclaiming free blocks, allow this many concurrent
    // GetObject+PutObject requests.
    static ref RECLAIM_QUEUE_DEPTH: usize = 200;

    // minimum number of chunks before we consider condensing
    static ref LOG_CONDENSE_MIN_CHUNKS: usize = 30;
    // when log is 5x as large as the condensed version
    static ref LOG_CONDENSE_MULTIPLE: usize = 5;

    pub static ref CLAIM_DURATION: Duration = Duration::from_secs(2);
    pub static ref CREATE_WAIT_DURATION: Duration = Duration::from_secs(30);

    // By default, retain metadata for as long as we would return Uberblocks in a block-based pool
    static ref METADATA_RETENTION_TXGS: u64 = 128;

    static ref WRITES_INGEST_TO_ZETTACACHE: bool = true;

    static ref SIBLING_BLOCKS_INGEST_TO_ZETTACACHE: bool = true;
}

lazy_static! {
    // Max concurrent GetObject's for a single object consolidation.
    static ref RECLAIM_ONE_BUFFERED: usize = *RECLAIM_QUEUE_DEPTH / 10 + 1;
}

const ONE_MIB: u64 = 1_048_576;

const BLOCKIDS_PER_LOG_GROUP: u64 = 1024 * 1024;
const RECLAIM_TABLE_MAX_BITS: u8 = 16;

enum OwnResult {
    Success,
    Failure(HeartbeatPhys),
    Retry,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
struct PoolOwnerPhys {
    id: PoolGuid,
    owner: Uuid,
}

impl PoolOwnerPhys {
    fn key(id: PoolGuid) -> String {
        format!("zfs/{}/owner", id)
    }

    async fn get(object_access: &ObjectAccess, id: PoolGuid) -> Result<Self> {
        let buf = object_access
            .get_object(Self::key(id), ObjectAccessOpType::MetadataGet)
            .await?;
        let this: Self = from_json_slice(&buf)
            .with_context(|| format!("Failed to decode contents of {}", Self::key(id)))?;
        debug!("got {:#?}", this);
        assert_eq!(this.id, id);
        Ok(this)
    }

    async fn put_timeout(
        &self,
        object_access: &ObjectAccess,
        timeout: Option<Duration>,
    ) -> Result<(), OAError<PutError>> {
        maybe_die_with(|| format!("before putting {:#?}", self));
        debug!("putting {:#?}", self);
        let buf = serde_json::to_vec(&self).unwrap();
        object_access
            .put_object_timed(
                Self::key(self.id),
                buf.into(),
                ObjectAccessOpType::MetadataPut,
                timeout,
            )
            .await
    }

    async fn delete(object_access: &ObjectAccess, id: PoolGuid) {
        object_access.delete_object(Self::key(id)).await;
    }
}
#[derive(Debug)]
pub enum PoolOpenError {
    Mmp(String),
    Feature(FeatureError),
    Get(Error),
    NoCheckpoint,
}

impl From<anyhow::Error> for PoolOpenError {
    fn from(e: anyhow::Error) -> Self {
        PoolOpenError::Get(e)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PoolPhys {
    pub guid: PoolGuid, // redundant with key, for verification
    pub name: String,
    last_txg: Txg,
    pub destroying_state: Option<PoolDestroyingPhys>,
    #[serde(default)]
    checkpoint_txg: Option<Txg>,
}

/// contains a pending_frees_log and matching object_size_log
#[derive(Serialize, Deserialize, Debug)]
struct ReclaimLogPhys {
    num_bits: u8, // aka local depth; range is [0, 16], inclusive
    prefix: u16,  // prefix used to locate this log in table
    pending_frees_log: ObjectBasedLogPhys<PendingFreesLogEntry>,
    pending_free_bytes: u64,
    object_size_log: ObjectBasedLogPhys<ObjectSizeLogEntry>,
}

/// Metadata for reclaiming freed blocks
#[derive(Serialize, Deserialize, Debug)]
struct ReclaimInfoPhys {
    indirect_table: Vec<ReclaimLogId>, // has at least one entry and size is always a power of two
    reclaim_logs: Vec<ReclaimLogPhys>,
}

impl ReclaimInfoPhys {
    fn table_bits(&self) -> u8 {
        let length = &self.indirect_table.len();
        assert!(length.is_power_of_two());
        u8::try_from(length.trailing_zeros()).unwrap()
    }
}

#[derive(Serialize, Deserialize, Derivative)]
#[derivative(Debug)]
pub struct UberblockPhys {
    guid: PoolGuid,   // redundant with key, for verification
    txg: Txg,         // redundant with key, for verification
    date: SystemTime, // for debugging
    storage_object_log: ObjectBasedLogPhys<StorageObjectLogEntry>,
    #[derivative(Debug = "ignore")]
    reclaim_info: ReclaimInfoPhys, // Extendible hash structures for reclaiming free blocks.
    next_block: BlockId, // Next BlockID that can be allocated.
    obsolete_objects: ObjectDeleterPhys,
    stats: PoolStatsPhys,
    features: Vec<(FeatureFlag, u64)>, // Each pair is a feature and its refcount
    #[derivative(Debug(format_with = "util::tersevec"))]
    zfs_uberblock: Vec<u8>,
    #[derivative(Debug(format_with = "util::tersevec"))]
    zfs_config: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy)]
pub struct PoolStatsPhys {
    pub blocks_count: u64, // Note: does not include the pending_object
    pub blocks_bytes: u64, // Note: does not include the pending_object
    pub pending_frees_count: u64,
    pub pending_frees_bytes: u64,
    // XXX shouldn't really be needed on disk since we always have the storage_object_log loaded
    // into the `objects` field
    pub objects_count: u64,
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
enum ObjectSizeLogEntry {
    Exists(ObjectSize),
    Freed { object: ObjectId },
}
impl ObjectBasedLogEntry for ObjectSizeLogEntry {}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectSize {
    object: ObjectId,
    num_blocks: u32,
    num_bytes: u32, // bytes in blocks; does not include Agent metadata
}

/// This lets us use an ObjectId to lookup in a Map/Set of ObjectSize's.  There
/// must not be two ObjectSize structs that have the same ObjectID but different
/// blocks/bytes (that are part of the same Map/Set).
impl Borrow<ObjectId> for ObjectSize {
    fn borrow(&self) -> &ObjectId {
        &self.object
    }
}

impl From<&DataObject> for ObjectSize {
    fn from(phys: &DataObject) -> Self {
        ObjectSize {
            object: phys.header.object,
            num_blocks: phys.blocks_len(),
            num_bytes: phys.header.blocks_size,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq)]
struct PendingFreesLogEntry {
    block: BlockId,
    size: u32, // in bytes
}
impl ObjectBasedLogEntry for PendingFreesLogEntry {}

/*
 * Accessors for on-disk structures
 */

impl PoolPhys {
    pub fn key(guid: PoolGuid) -> String {
        format!("zfs/{}/super", guid)
    }

    pub async fn exists(object_access: &ObjectAccess, guid: PoolGuid) -> bool {
        object_access.object_exists(Self::key(guid)).await
    }

    pub async fn get(object_access: &ObjectAccess, guid: PoolGuid) -> Result<Self> {
        let buf = object_access
            .get_object(Self::key(guid), ObjectAccessOpType::MetadataGet)
            .await?;
        let this: Self = from_json_slice(&buf)
            .with_context(|| format!("Failed to decode contents of {}", Self::key(guid)))?;
        debug!("got {:#?}", this);
        assert_eq!(this.guid, guid);
        Ok(this)
    }

    pub async fn put(&self, object_access: &ObjectAccess) {
        maybe_die_with(|| format!("before putting {:#?}", self));
        debug!("putting {:#?}", self);
        let buf = serde_json::to_vec(&self).unwrap();
        object_access
            .put_object(
                Self::key(self.guid),
                buf.into(),
                ObjectAccessOpType::MetadataPut,
            )
            .await;
    }

    pub async fn put_timed(
        &self,
        object_access: &ObjectAccess,
        timeout: Option<Duration>,
    ) -> Result<(), OAError<PutError>> {
        maybe_die_with(|| format!("before putting {:#?}", self));
        debug!("putting {:#?}", self);
        let buf = serde_json::to_vec(&self).unwrap();
        object_access
            .put_object_timed(
                Self::key(self.guid),
                buf.into(),
                ObjectAccessOpType::MetadataPut,
                timeout,
            )
            .await
    }
}

impl UberblockPhys {
    fn key(guid: PoolGuid, txg: Txg) -> String {
        format!("zfs/{}/txg/{}", guid, txg)
    }

    pub fn zfs_uberblock(&self) -> &[u8] {
        &self.zfs_uberblock
    }

    pub fn zfs_config(&self) -> &[u8] {
        &self.zfs_config
    }

    // Each pair is a featureflag and its refcount.
    pub fn features(&self) -> &Vec<(FeatureFlag, u64)> {
        &self.features
    }

    pub async fn get(object_access: &ObjectAccess, guid: PoolGuid, txg: Txg) -> Result<Self> {
        let buf = object_access
            .get_object(Self::key(guid, txg), ObjectAccessOpType::MetadataGet)
            .await?;
        let this: Self = from_json_slice(&buf)
            .with_context(|| format!("Failed to decode contents of {}", Self::key(guid, txg)))?;
        debug!("got {:#?}", this);
        assert_eq!(this.guid, guid);
        assert_eq!(this.txg, txg);
        Ok(this)
    }

    async fn put(&self, object_access: &ObjectAccess) {
        maybe_die_with(|| format!("before putting {:#?}", self));
        debug!("putting {:#?}", self);
        let buf = serde_json::to_vec(&self).unwrap();
        object_access
            .put_object(
                Self::key(self.guid, self.txg),
                buf.into(),
                ObjectAccessOpType::MetadataPut,
            )
            .await;
    }

    async fn cleanup_older_uberblocks(object_access: &ObjectAccess, ub: UberblockPhys) {
        object_access
            .delete_objects(
                object_access
                    .list_objects(format!("zfs/{}/txg/", ub.guid), true)
                    .map(|prefix| Txg::from_key(prefix.as_str()))
                    .filter(|&txg| future::ready(txg < ub.txg))
                    .inspect(|txg| debug!("deleting old uberblock {txg:?}"))
                    .map(|txg| Self::key(ub.guid, txg)),
            )
            .await;
    }
}

/*
 * Main storage pool interface
 */

pub struct Pool {
    pub state: Arc<PoolState>,
}

pub struct PoolState {
    syncing_state: std::sync::Mutex<Option<PoolSyncingState>>,
    object_block_map: ObjectBlockMap,
    zettacache: Arc<ZettaCache>,
    pub shared_state: Arc<PoolSharedState>,
    resuming: watch_once::Receiver<()>,
    heartbeat_guard: Option<HeartbeatGuard>,
}

/// runtime data for each pair of pending frees + object sizes logs
struct ReclaimLog {
    reclaim_busy: bool, // being reclaimed (skip splitting)
    num_bits: u8,       // aka local depth and range is { 0, 16 }
    prefix: u16,        // prefix used to locate this log in table
    id: ReclaimLogId,

    // Note: the pending_frees_log may contain frees that were already applied,
    // if we crashed while processing pending frees.
    pending_frees_log: ObjectBasedLog<PendingFreesLogEntry>,
    pending_free_bytes: u64,
    // Note: the object_size_log may not have the most up-to-date size info for
    // every object, because it's updated after the object is overwritten, when
    // processing pending frees.
    object_size_log: ObjectBasedLog<ObjectSizeLogEntry>,
}

impl ReclaimLog {
    fn to_phys(&self) -> ReclaimLogPhys {
        ReclaimLogPhys {
            num_bits: self.num_bits,
            prefix: self.prefix,
            pending_frees_log: self.pending_frees_log.to_phys(),
            pending_free_bytes: self.pending_free_bytes,
            object_size_log: self.object_size_log.to_phys(),
        }
    }
}

struct ReclaimIndirectTable {
    table_bits: u8,             // aka global depth; range is [0, 16], inclusive
    log_ids: Vec<ReclaimLogId>, // has at least one entry and size of table is 2 ^ table_bits
}

impl ReclaimIndirectTable {
    fn grow_table(&mut self) {
        assert_lt!(self.table_bits, RECLAIM_TABLE_MAX_BITS);

        let old_len = self.log_ids.len();
        let mut new_log_ids = Vec::with_capacity(old_len * 2);
        for &index in &self.log_ids {
            new_log_ids.push(index);
            new_log_ids.push(index);
        }
        self.log_ids = new_log_ids;
        self.table_bits += 1;

        info!(
            "reclaim: growing indirect table from {} to {}",
            old_len,
            self.log_ids.len()
        );

        assert_eq!(self.log_ids.len(), 1 << self.table_bits);
    }

    /// Update the sibling indices to point to a new log
    fn update_siblings(&mut self, log_bits: u8, log_prefix: u16, new_log_id: ReclaimLogId) {
        // Note: there will always be a power of 2 amount of siblings to update
        let prefix_diff = self.table_bits - log_bits;
        let sibling_index = usize::from(log_prefix << prefix_diff);
        let bit_width = self.table_bits as usize;
        for i in 0..(1 << prefix_diff) {
            self.log_ids[sibling_index + i] = new_log_id;
            debug!(
                "reclaim: update sibling[{:#0width$b}] = {}",
                sibling_index + i,
                new_log_id.0,
                width = bit_width + 2,
            );
        }
    }
}

impl Display for ReclaimIndirectTable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "ReclaimIndirectTable[{} bits]", self.table_bits)?;
        let bits = self.table_bits as usize;
        for (i, v) in self.log_ids.iter().enumerate() {
            writeln!(f, "   log_ids[{:#0width$b}] = {}", i, v, width = bits + 2)?;
        }
        Ok(())
    }
}

/// runtime data for reclaiming pending frees
struct ReclaimInfo {
    indirect_table: ReclaimIndirectTable,
    reclaim_logs: Vec<ReclaimLog>,
}

impl ReclaimInfo {
    fn to_phys(&self) -> ReclaimInfoPhys {
        ReclaimInfoPhys {
            indirect_table: self.indirect_table.log_ids.clone(),
            reclaim_logs: self.reclaim_logs.iter().map(|log| log.to_phys()).collect(),
        }
    }
}

type WriteCallback = Box<dyn FnOnce() + Send>;

/// state that's modified while syncing a txg
//#[derive(Debug)]
struct PoolSyncingState {
    // Note: some objects may contain additional (adjacent) blocks, if they have
    // been consolidated but this fact is not yet represented in the log.  A
    // consolidated object won't be removed until after the log reflects that.
    // XXX put this in its own type (in object_block_map.rs?)
    storage_object_log: ObjectBasedLog<StorageObjectLogEntry>,

    reclaim_info: ReclaimInfo, // Extendible hash structure for pending frees

    pending_object: PendingObjectState,
    pending_unordered_writes: Unordered<BlockId, (Bytes, WriteCallback)>,
    pub last_txg: Txg,
    pub syncing_txg: Option<Txg>,
    stats: PoolStatsPhys,
    reclaim_done: Option<oneshot::Receiver<SyncTask>>,
    object_deleter: ObjectDeleter,
    // Flush immediately once we have one of these blocks (and all previous blocks)
    pending_flushes: BTreeSet<BlockId>,
    cleanup_handle: Option<JoinHandle<()>>,
    features: HashMap<FeatureFlag, u64>,
    resuming: Option<watch_once::Sender<()>>,
    checkpoint_txg: Option<Txg>,
}

type SyncTask =
    Box<dyn FnOnce(&mut PoolSyncingState) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> + Send>;

#[derive(Derivative)]
#[derivative(Debug)]
enum PendingObjectState {
    // available to write
    Pending(
        DataObject,
        #[derivative(Debug(format_with = "util::tersevec"))] Vec<WriteCallback>,
    ),
    // not available to write; this is the next blockID to use
    NotPending(BlockId),
}

impl PendingObjectState {
    fn as_mut_pending(&mut self) -> (&mut DataObject, &mut Vec<WriteCallback>) {
        match self {
            PendingObjectState::Pending(phys, done) => (phys, done),
            _ => panic!("invalid {:?}", self),
        }
    }

    fn unwrap_pending(self) -> (DataObject, Vec<WriteCallback>) {
        match self {
            PendingObjectState::Pending(phys, done) => (phys, done),
            _ => panic!("invalid {:?}", self),
        }
    }

    fn is_pending(&self) -> bool {
        match self {
            PendingObjectState::Pending(..) => true,
            PendingObjectState::NotPending(..) => false,
        }
    }

    fn next_block(&self) -> BlockId {
        match self {
            PendingObjectState::Pending(phys, _) => phys.header.next_block,
            PendingObjectState::NotPending(next_block) => *next_block,
        }
    }

    fn new_pending(guid: PoolGuid, object: ObjectId, next_block: BlockId, txg: Txg) -> Self {
        PendingObjectState::Pending(DataObject::new(guid, object, next_block, txg), Vec::new())
    }
}

/*
 * Note: this struct is passed to the OBL code.  It needs to be a separate struct from Pool,
 * because it can't refer back to the OBL itself, which would create a circular reference.
 */
pub struct PoolSharedState {
    pub object_access: Arc<ObjectAccess>,
    pub guid: PoolGuid,
    pub name: String,
}

impl PoolSyncingState {
    fn next_block(&self) -> BlockId {
        self.pending_object.next_block()
    }

    fn log_free(&mut self, ent: PendingFreesLogEntry, object_block_map: &ObjectBlockMap) {
        let txg = self.syncing_txg.unwrap();
        assert_lt!(ent.block, self.next_block());

        let log = self.get_pending_frees_log_for_obj(object_block_map.block_to_object(ent.block));
        log.pending_frees_log.append(txg, ent);
        log.pending_free_bytes += u64::from(ent.size);

        self.stats.pending_frees_count += 1;
        self.stats.pending_frees_bytes += u64::from(ent.size);
    }

    /// Locate which log to use for this object.
    fn get_log_id(&mut self, object: ObjectId) -> ReclaimLogId {
        // Note: All the freed blocks from the same object land in the same log

        // Group adjacent objects together
        let object_group = object.as_min_block().0 / BLOCKIDS_PER_LOG_GROUP;

        let hash_value = u16::try_from(object_group % (1 << RECLAIM_TABLE_MAX_BITS))
            .unwrap()
            .reverse_bits();
        let table = &self.reclaim_info.indirect_table;
        let index = usize::from(hash_value) >> (RECLAIM_TABLE_MAX_BITS - table.table_bits);
        table.log_ids[index]
    }

    /// Calculate the range of continuous objects that are part of the same log
    /// as the specified object.  Returns [min, max) (i.e. min is inclusive, max
    /// is exclusive)
    fn get_log_range(object: ObjectId) -> (ObjectId, ObjectId) {
        let min = ObjectId::new(BlockId(
            object.as_min_block().0 / BLOCKIDS_PER_LOG_GROUP * BLOCKIDS_PER_LOG_GROUP,
        ));
        let max = ObjectId::new(BlockId(min.as_min_block().0 + BLOCKIDS_PER_LOG_GROUP));

        assert_le!(min, object);
        assert_gt!(max, object);
        /*
        let log = self.get_log_id(object);
        assert_ne!(self.get_log_id(ObjectId(min.0 - 1)), log);
        assert_eq!(self.get_log_id(min), log);
        assert_eq!(self.get_log_id(ObjectId(max.0 - 1)), log);
        assert_ne!(self.get_log_id(max), log);
        */

        (min, max)
    }

    fn get_pending_frees_log(&mut self, log: ReclaimLogId) -> &mut ReclaimLog {
        &mut self.reclaim_info.reclaim_logs[log.as_index()]
    }

    fn get_pending_frees_log_for_obj(&mut self, object: ObjectId) -> &mut ReclaimLog {
        let log = self.get_log_id(object);
        &mut self.reclaim_info.reclaim_logs[log.as_index()]
    }

    #[allow(dead_code)]
    fn feature_is_enabled(&mut self, flag: &FeatureFlag) -> bool {
        self.features.contains_key(flag)
    }

    #[allow(dead_code)]
    fn feature_is_active(&mut self, flag: &FeatureFlag) -> bool {
        self.features.get(flag).map_or(false, |x| *x > 0)
    }

    #[allow(dead_code)]
    fn feature_enable(&mut self, flag: FeatureFlag) {
        assert!(self.features.insert(flag, 0).is_none());
    }

    #[allow(dead_code)]
    fn feature_disable(&mut self, flag: &FeatureFlag) {
        assert!(self.features.remove(flag).unwrap() == 0);
    }

    #[allow(dead_code)]
    fn feature_increment_refcount(&mut self, flag: &FeatureFlag) {
        *self.features.get_mut(flag).unwrap() += 1;
    }

    #[allow(dead_code)]
    fn feature_decrement_refcount(&mut self, flag: &FeatureFlag) {
        *self.features.get_mut(flag).unwrap() -= 1;
    }
}

impl PoolState {
    // XXX change this to return a Result.  If the syncing_state is None, return
    // an error which means that we're in the middle of end_txg.
    fn with_syncing_state<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut PoolSyncingState) -> R,
    {
        let mut guard = self.syncing_state.lock().unwrap();
        f(guard.as_mut().unwrap())
    }

    /// Remove any data objects that were created as part of an in-progress txg
    /// before the kernel crashed.
    async fn cleanup_data_objects(&self) {
        let shared_state = self.shared_state.clone();
        let oa = &shared_state.object_access;
        let begin = Instant::now();
        let last_obj = self.object_block_map.last_object();
        let mut count: u32 = 0;

        oa.delete_objects(
            select_all(DataObject::prefixes(shared_state.guid).map(|prefix| {
                let start_after = format!("{}{}", prefix, last_obj);
                match oa.try_list_after(prefix, true, start_after) {
                    Some(stream) => stream.left_stream(),
                    None => stream::empty().right_stream(),
                }
                .boxed()
            }))
            .inspect(|_| count += 1),
        )
        .await;

        info!(
            "cleanup: found and deleted {} data objects in {}ms",
            count,
            begin.elapsed().as_millis()
        );
    }

    pub fn object_block_set(&self) -> BTreeSet<ObjectId> {
        self.object_block_map.raw_set()
    }
}

impl Pool {
    pub async fn exists(object_access: &ObjectAccess, guid: PoolGuid) -> bool {
        PoolPhys::exists(object_access, guid).await
    }

    pub async fn get_config(object_access: &ObjectAccess, guid: PoolGuid) -> Result<NvList> {
        let pool_phys = PoolPhys::get(object_access, guid).await?;
        let uberblock_phys =
            UberblockPhys::get(object_access, pool_phys.guid, pool_phys.last_txg).await?;
        let nvl = NvList::try_unpack(&uberblock_phys.zfs_config)?;
        Ok(nvl)
    }

    pub async fn create(
        object_access: &ObjectAccess,
        name: &str,
        guid: PoolGuid,
    ) -> Result<(), OAError<PutError>> {
        let phys = PoolPhys {
            guid,
            name: name.to_string(),
            last_txg: Txg(0),
            destroying_state: None,
            checkpoint_txg: None,
        };
        // XXX make sure it doesn't already exist
        phys.put_timed(object_access, Some(*CREATE_WAIT_DURATION))
            .await
    }

    async fn open_from_txg(
        object_access: Arc<ObjectAccess>,
        pool_phys: &PoolPhys,
        txg: Txg,
        zettacache: Arc<ZettaCache>,
        heartbeat_guard: Option<HeartbeatGuard>,
        readonly: bool,
        mut syncing_txg: Option<Txg>,
    ) -> Result<(Pool, UberblockPhys, BlockId), PoolOpenError> {
        let phys = UberblockPhys::get(&object_access, pool_phys.guid, txg).await?;

        features::check_features(phys.features.iter().map(|(f, _)| f), readonly)?;

        if let Some(resume_txg) = syncing_txg {
            assert_ge!(resume_txg, phys.txg);
            if resume_txg == phys.txg {
                // The TXG that we're resuming was already synced.  The agent
                // must have died before the "end txg" response got to the
                // kernel.  To ensure that the next message is "end txg" (not
                // "write block"), we set the syncing_txg to None.  See
                // end_txg() for details.
                syncing_txg = None;
            }
        }

        let shared_state = Arc::new(PoolSharedState {
            object_access: object_access.clone(),
            guid: pool_phys.guid,
            name: pool_phys.name.clone(),
        });

        // load block -> object mapping
        let storage_object_log =
            ObjectBasedLog::open_by_phys(shared_state.clone(), &phys.storage_object_log);
        let object_block_map = ObjectBlockMap::load(&storage_object_log, phys.next_block).await;
        let mut logs = Vec::new();
        for (i, log_phys) in phys.reclaim_info.reclaim_logs.iter().enumerate() {
            logs.push(ReclaimLog {
                reclaim_busy: false,
                num_bits: log_phys.num_bits,
                prefix: log_phys.prefix,
                id: ReclaimLogId(u16::try_from(i).unwrap()),
                pending_frees_log: ObjectBasedLog::open_by_phys(
                    shared_state.clone(),
                    &log_phys.pending_frees_log,
                ),
                pending_free_bytes: log_phys.pending_free_bytes,
                object_size_log: ObjectBasedLog::open_by_phys(
                    shared_state.clone(),
                    &log_phys.object_size_log,
                ),
            });
        }

        let (tx, rx) = watch_once::channel();
        // If not syncing, drop Sender so that Receivers will return immediately
        let tx = syncing_txg.map(|_| tx);
        /*
         * If we are rolling backwards to a checkpoint and the checkpoint txg is equal
         * to the current txg, then delete the checkpoint and continue onwards. In all
         * other cases, the checkpoint txg is at least one TXG before the current txg.
         */
        let checkpoint_txg = if pool_phys.checkpoint_txg == Some(txg) {
            None
        } else {
            pool_phys.checkpoint_txg
        };
        let pool = Pool {
            state: Arc::new(PoolState {
                shared_state: shared_state.clone(),
                syncing_state: std::sync::Mutex::new(Some(PoolSyncingState {
                    last_txg: phys.txg,
                    syncing_txg,
                    storage_object_log,
                    reclaim_info: ReclaimInfo {
                        indirect_table: ReclaimIndirectTable {
                            table_bits: phys.reclaim_info.table_bits(),
                            log_ids: phys.reclaim_info.indirect_table.clone(),
                        },
                        reclaim_logs: logs,
                    },
                    pending_object: PendingObjectState::NotPending(phys.next_block),
                    pending_unordered_writes: Unordered::new(phys.next_block),
                    stats: phys.stats,
                    reclaim_done: None,
                    object_deleter: ObjectDeleter::open(
                        object_access.clone(),
                        pool_phys.guid,
                        phys.obsolete_objects.clone(),
                    ),
                    pending_flushes: Default::default(),
                    cleanup_handle: None,
                    features: phys.features.iter().cloned().collect(),
                    resuming: tx,
                    checkpoint_txg,
                })),
                zettacache,
                object_block_map,
                resuming: rx,
                heartbeat_guard,
            }),
        };

        let syncing_state = {
            let mut guard = pool.state.syncing_state.lock().unwrap();
            guard.take().unwrap()
        };

        assert_eq!(
            pool.state.object_block_map.len() as u64,
            syncing_state.stats.objects_count
        );

        let next_block = syncing_state.next_block();

        *pool.state.syncing_state.lock().unwrap() = Some(syncing_state);
        Ok((pool, phys, next_block))
    }

    pub async fn open(
        object_access: Arc<ObjectAccess>,
        guid: PoolGuid,
        txg: Option<Txg>,
        zettacache: Arc<ZettaCache>,
        id: Uuid,
        syncing_txg: Option<Txg>,
        rollback: bool,
    ) -> Result<(Pool, Option<UberblockPhys>, BlockId), PoolOpenError> {
        let phys = PoolPhys::get(&object_access, guid).await?;
        if phys.last_txg.0 == 0 {
            assert!(!rollback);
            let shared_state = Arc::new(PoolSharedState {
                object_access: object_access.clone(),
                guid,
                name: phys.name,
            });

            let storage_object_log = ObjectBasedLog::create(
                shared_state.clone(),
                &format!("zfs/{}/StorageObjectLog", guid),
            );
            let object_block_map = ObjectBlockMap::load(&storage_object_log, BlockId(0)).await;

            // start with a table of size 1 and a single log
            let mut logs = Vec::new();
            let log_id = ReclaimLogId(0);
            logs.push(ReclaimLog {
                reclaim_busy: false,
                num_bits: 0, // starts with a table of 1 entry
                prefix: 0,
                id: log_id,
                pending_frees_log: ObjectBasedLog::create(
                    shared_state.clone(),
                    &format!("zfs/{}/PendingFreesLog/{}", guid, log_id),
                ),
                pending_free_bytes: 0,
                object_size_log: ObjectBasedLog::create(
                    shared_state.clone(),
                    &format!("zfs/{}/ObjectSizeLog/{}", guid, log_id),
                ),
            });

            let mut pool = Pool {
                state: Arc::new(PoolState {
                    shared_state: shared_state.clone(),
                    syncing_state: std::sync::Mutex::new(Some(PoolSyncingState {
                        last_txg: Txg(0),
                        syncing_txg: None,
                        storage_object_log,
                        reclaim_info: ReclaimInfo {
                            indirect_table: ReclaimIndirectTable {
                                table_bits: 0,
                                log_ids: vec![ReclaimLogId(0)],
                            },
                            reclaim_logs: logs,
                        },
                        pending_object: PendingObjectState::NotPending(BlockId(0)),
                        pending_unordered_writes: Unordered::new(BlockId(0)),
                        stats: Default::default(),
                        reclaim_done: None,
                        object_deleter: ObjectDeleter::new(object_access.clone(), guid),
                        pending_flushes: Default::default(),
                        cleanup_handle: None,
                        features: Default::default(),
                        resuming: None,
                        checkpoint_txg: None,
                    })),
                    zettacache,
                    object_block_map,
                    resuming: watch_once::channel().1,
                    heartbeat_guard: if !shared_state.object_access.readonly() {
                        Some(
                            heartbeat::start_heartbeat(shared_state.object_access.clone(), id)
                                .await,
                        )
                    } else {
                        None
                    },
                }),
            };

            let next_block = pool
                .state
                .with_syncing_state(|syncing_state| syncing_state.next_block());
            pool.claim(id).await.map(|_| (pool, None, next_block))
        } else {
            let target = if rollback {
                phys.checkpoint_txg.ok_or(PoolOpenError::NoCheckpoint)?
            } else {
                txg.unwrap_or(phys.last_txg)
            };
            let (mut pool, ub, next_block) = Pool::open_from_txg(
                object_access.clone(),
                &phys,
                target,
                zettacache,
                if !object_access.readonly() {
                    Some(heartbeat::start_heartbeat(object_access.clone(), id).await)
                } else {
                    None
                },
                object_access.readonly(),
                syncing_txg,
            )
            .await?;

            pool.claim(id).await?;

            if !object_access.readonly() {
                let last_txg = pool
                    .state
                    .with_syncing_state(|syncing_state| syncing_state.last_txg);
                if last_txg != phys.last_txg {
                    // We opened an older TXG.  Before cleaning up (deleting)
                    // future TXG's, update the super object to the old TXG, so
                    // that if we re-open the pool we don't try to use the
                    // future (deleted) TXG.
                    let new_phys = PoolPhys { last_txg, ..phys };
                    new_phys.put(&object_access).await;
                }
                if syncing_txg.is_none() {
                    pool.state.cleanup_data_objects().await;
                }
            }
            Ok((pool, Some(ub), next_block))
        }
    }

    async fn get_recovered_objects(
        state: &Arc<PoolState>,
        shared_state: &Arc<PoolSharedState>,
        final_write: BlockId,
    ) -> BTreeMap<ObjectId, DataObject> {
        if shared_state.object_access.supports_list_after() {
            let recovered = recover_list(state, shared_state).await;
            assert!(recovered
                .iter()
                .next_back()
                .map(|(k, _)| k.as_min_block() <= final_write)
                .unwrap_or(true));
            return recovered;
        }

        let mut recovered = BTreeMap::new();
        let (last_object, _) = DataObject::get(
            &shared_state.object_access,
            shared_state.guid,
            state.object_block_map.last_object(),
        )
        .await
        .unwrap();

        let mut next_id = ObjectId::new(last_object.header.next_block);
        loop {
            while let Ok(object) = DataObject::get_uncached(
                &shared_state.object_access,
                shared_state.guid,
                next_id,
                ObjectAccessOpType::ReadsGet,
            )
            .await
            {
                next_id = ObjectId::new(object.header.next_block);
                recovered.insert(object.header.object, object);
            }
            if next_id.as_min_block() >= final_write {
                break;
            }
            if let Some(object) = DataObject::next_uncached(
                &shared_state.object_access,
                shared_state.guid,
                next_id,
                ObjectId::new(final_write),
            )
            .await
            {
                next_id = ObjectId::new(object.header.next_block);
                recovered.insert(object.header.object, object);
            } else {
                break;
            }
        }

        recovered
    }

    pub async fn resume_complete(&self) {
        let state = &self.state;
        let (txg, final_write) = self.state.with_syncing_state(|syncing_state| {
            // verify that we're in resuming state
            assert!(!syncing_state.pending_object.is_pending());
            (
                syncing_state.syncing_txg.unwrap(),
                syncing_state.pending_unordered_writes.last(),
            )
        });
        let shared_state = &state.shared_state;

        let recovered_objects = Self::get_recovered_objects(state, shared_state, final_write).await;

        self.state.with_syncing_state(|syncing_state| {
            let mut recovered_objects_iter = recovered_objects.into_iter().peekable();

            while let Some((_, next_recovered_object)) = recovered_objects_iter.peek() {
                match syncing_state.pending_unordered_writes.peek() {
                    Some(next_ordered_write)
                        if next_ordered_write
                            < next_recovered_object.header.object.as_min_block() =>
                    {
                        // writes are next, and there are objects after this

                        assert!(!syncing_state.pending_object.is_pending());
                        syncing_state.pending_object = PendingObjectState::new_pending(
                            self.state.shared_state.guid,
                            state.object_block_map.next_object(),
                            syncing_state.pending_object.next_block(),
                            txg,
                        );

                        // XXX Unless there is already an object at object.next(), we
                        // should limit the object size as normal.
                        Self::write_unordered_to_pending_object(
                            state,
                            syncing_state,
                            None,
                            Some(next_recovered_object.header.object.as_min_block()),
                        );

                        let (phys, _) = syncing_state.pending_object.as_mut_pending();
                        debug!("resume: writes are next; creating {}", phys);

                        Self::initiate_flush_object_impl(state, syncing_state);
                        let next_block = syncing_state.pending_object.next_block();
                        syncing_state.pending_object = PendingObjectState::NotPending(next_block);
                    }
                    _ => {
                        // already-written object is next

                        let (_, recovered_obj) = recovered_objects_iter.next().unwrap();
                        debug!("resume: next is {}", recovered_obj);

                        Self::account_new_object(state, syncing_state, &recovered_obj);

                        // The kernel may not have known that this was already written (e.g. we
                        // didn't quite get to sending the "write done" response), so it sent us
                        // the write again.  In this case we will not create an object, since the
                        // blocks are already persistent, so we need to notify the waiter now.
                        for (obsolete_write, (_, callback)) in syncing_state
                            .pending_unordered_writes
                            .drain(recovered_obj.header.next_block)
                        {
                            trace!(
                                "resume: {:?} is obsoleted by existing {:?}",
                                obsolete_write,
                                recovered_obj.header.object,
                            );
                            callback();
                        }
                        assert!(!syncing_state.pending_object.is_pending());
                        syncing_state.pending_object =
                            PendingObjectState::NotPending(recovered_obj.header.next_block);
                    }
                }
            }

            // no recovered objects left; move ordered portion of writes to pending_object
            assert!(!syncing_state.pending_object.is_pending());
            syncing_state.pending_object = PendingObjectState::new_pending(
                self.state.shared_state.guid,
                state.object_block_map.next_object(),
                syncing_state.pending_object.next_block(),
                txg,
            );

            debug!("resume: moving last writes to pending_object and flushing");
            Self::write_unordered_to_pending_object(
                state,
                syncing_state,
                Some(TARGET_OBJECT_SIZE.as_u64().try_into().unwrap()),
                None,
            );
            Self::initiate_flush_object_impl(state, syncing_state);

            // drop the Sender, causing waiting Receivers to wake up
            assert!(syncing_state.resuming.is_some());
            syncing_state.resuming = None;

            info!("resume: completed");
        })
    }

    pub fn begin_txg(&self, txg: Txg) {
        self.state.with_syncing_state(|syncing_state| {
            // XXX change this to return an error to the client
            assert!(syncing_state.syncing_txg.is_none());
            assert_gt!(txg, syncing_state.last_txg);
            syncing_state.syncing_txg = Some(txg);

            assert!(!syncing_state.pending_object.is_pending());
            syncing_state.pending_object = PendingObjectState::new_pending(
                self.state.shared_state.guid,
                self.state.object_block_map.next_object(),
                syncing_state.pending_object.next_block(),
                txg,
            );
        })
    }

    pub async fn end_txg(
        &self,
        zfs_uberblock: Vec<u8>,
        zfs_config: Vec<u8>,
        checkpoint_txg: Option<Txg>,
    ) -> (PoolStatsPhys, Vec<(FeatureFlag, u64)>) {
        let state = &self.state;

        let mut syncing_state = state.syncing_state.lock().unwrap().take().unwrap();

        if syncing_state.syncing_txg.is_none() {
            // Note: if we died after writing the super object but before the
            // kernel got the "end txg" response, it will resume the last
            // completed txg.  In this case we're syncing the last txg again.
            // This should be a no-op.
            let phys = UberblockPhys::get(
                &state.shared_state.object_access,
                state.shared_state.guid,
                syncing_state.last_txg,
            )
            .await
            .unwrap();
            assert_eq!(phys.zfs_uberblock, zfs_uberblock);
            assert_eq!(phys.zfs_config, zfs_config);

            assert!(!syncing_state.pending_object.is_pending());
            assert!(syncing_state.pending_unordered_writes.is_empty());

            // We could refactor this into a separate function and define it as a no-op unless
            // debug_assertions is configured, if we want this to only happen in debug mode.
            let pool_phys = PoolPhys::get(
                &state.shared_state.object_access,
                self.state.shared_state.guid,
            )
            .await
            .unwrap();
            assert_eq!(pool_phys.checkpoint_txg, checkpoint_txg);
            assert_eq!(pool_phys.last_txg, syncing_state.last_txg);
            assert!(pool_phys.destroying_state.is_none());

            let stats = syncing_state.stats;

            let feature_vec = syncing_state
                .features
                .iter()
                .map(|(f, r)| (f.clone(), *r))
                .collect();

            // put syncing_state back in the Option
            assert!(state.syncing_state.lock().unwrap().is_none());
            *state.syncing_state.lock().unwrap() = Some(syncing_state);

            return (stats, feature_vec);
        }

        // should have already been flushed; no pending writes
        assert!(syncing_state.pending_unordered_writes.is_empty());
        {
            let (phys, senders) = syncing_state.pending_object.as_mut_pending();
            assert!(phys.is_empty());
            assert!(senders.is_empty());

            syncing_state.pending_object = PendingObjectState::NotPending(phys.header.next_block);
        }

        if syncing_state.checkpoint_txg.is_none() && checkpoint_txg.is_some() {
            // When a checkpoint is created, it is for the TXG *before* the one currently syncing.
            assert_eq!(
                checkpoint_txg,
                syncing_state.syncing_txg.unwrap().checked_sub(1)
            );
        } else if syncing_state.checkpoint_txg.is_none() == checkpoint_txg.is_none() {
            // Either there's no checkpoint now or before, or there should be the same checkpoint
            // txg in both this txg and the previous one.
            assert_eq!(syncing_state.checkpoint_txg, checkpoint_txg);
        }

        syncing_state.checkpoint_txg = checkpoint_txg;

        try_split_reclaim_logs(state.clone(), &mut syncing_state).await;
        try_reclaim_frees(state.clone(), &mut syncing_state);
        try_condense_object_log(state.clone(), &mut syncing_state).await;
        if syncing_state
            .cleanup_handle
            .as_mut()
            .map_or(true, |handle| handle.now_or_never().is_some())
        {
            syncing_state.cleanup_handle = clean_metadata(state.clone(), &mut syncing_state);
        }

        let txg = syncing_state.syncing_txg.unwrap();

        if let Some(rt) = syncing_state.reclaim_done.as_mut() {
            if let Ok(cb) = rt.try_recv() {
                cb(&mut syncing_state).await;
            }
        }

        let frees_log_stream = FuturesUnordered::new();
        let size_log_stream = FuturesUnordered::new();
        for log in syncing_state.reclaim_info.reclaim_logs.iter_mut() {
            frees_log_stream.push(log.pending_frees_log.flush(txg));
            size_log_stream.push(log.object_size_log.flush(txg));
        }

        join3(
            syncing_state.storage_object_log.flush(txg),
            frees_log_stream.count(),
            size_log_stream.count(),
        )
        .await;

        syncing_state.storage_object_log.flush(txg).await;

        // write uberblock
        let u = UberblockPhys {
            guid: state.shared_state.guid,
            txg,
            date: SystemTime::now(),
            storage_object_log: syncing_state.storage_object_log.to_phys(),
            reclaim_info: syncing_state.reclaim_info.to_phys(),
            next_block: syncing_state.next_block(),
            obsolete_objects: syncing_state.object_deleter.phys(),
            zfs_uberblock,
            stats: syncing_state.stats,
            zfs_config,
            features: syncing_state
                .features
                .iter()
                .map(|(x, y)| (x.clone(), *y))
                .collect(),
        };
        u.put(&state.shared_state.object_access).await;

        // write super
        PoolPhys {
            guid: state.shared_state.guid,
            name: state.shared_state.name.clone(),
            last_txg: txg,
            destroying_state: None,
            checkpoint_txg,
        }
        .put(&state.shared_state.object_access)
        .await;

        // Now that the metadata state has been atomically moved forward, we
        // can delete objects that are no longer needed
        syncing_state.object_deleter.sync_done();

        // update txg
        syncing_state.last_txg = txg;
        syncing_state.syncing_txg = None;

        let stats = syncing_state.stats;

        let feature_vec = syncing_state
            .features
            .iter()
            .map(|(f, r)| (f.clone(), *r))
            .collect();

        // put syncing_state back in the Option
        assert!(state.syncing_state.lock().unwrap().is_none());
        *state.syncing_state.lock().unwrap() = Some(syncing_state);

        (stats, feature_vec)
    }

    fn check_pending_flushes(state: &PoolState, syncing_state: &mut PoolSyncingState) {
        let mut do_flush = false;
        let next_block = syncing_state
            .pending_object
            .as_mut_pending()
            .0
            .header
            .next_block;
        while let Some(flush_block_ref) = syncing_state.pending_flushes.iter().next() {
            let flush_block = *flush_block_ref;
            if flush_block < next_block {
                do_flush = true;
                syncing_state.pending_flushes.remove(&flush_block);
            } else {
                break;
            }
        }
        if do_flush {
            Self::initiate_flush_object_impl(state, syncing_state);
        }
    }

    // Begin writing out all blocks up to and including the given BlockID.  We
    // may not have called write_block() on all these blocks yet, but we will
    // soon.
    // Basically, as soon as we have this blockID and all the previous ones,
    // start writing that pending object immediately.
    pub fn initiate_flush(&self, block: BlockId) {
        self.state.with_syncing_state(|syncing_state| {
            trace!("flushing {:?}", block);
            if !syncing_state.pending_object.is_pending() {
                return;
            }

            syncing_state.pending_flushes.insert(block);
            Self::check_pending_flushes(&self.state, syncing_state);
        })
    }

    fn account_new_object(
        state: &PoolState,
        syncing_state: &mut PoolSyncingState,
        phys: &DataObject,
    ) {
        let txg = syncing_state.syncing_txg.unwrap();
        let object = phys.header.object;
        assert_eq!(phys.header.guid, state.shared_state.guid);
        assert_eq!(phys.header.min_txg, txg);
        assert_eq!(phys.header.max_txg, txg);
        syncing_state.stats.objects_count += 1;
        syncing_state.stats.blocks_bytes += u64::from(phys.header.blocks_size);
        syncing_state.stats.blocks_count += u64::from(phys.blocks_len());
        state
            .object_block_map
            .insert(object, phys.header.next_block);
        syncing_state
            .storage_object_log
            .append(txg, StorageObjectLogEntry::Alloc { object });
        syncing_state
            .get_pending_frees_log_for_obj(object)
            .object_size_log
            .append(txg, ObjectSizeLogEntry::Exists(phys.into()));
    }

    // completes when we've initiated the PUT to the object store.
    // callers should wait on the semaphore to ensure it's completed
    fn initiate_flush_object_impl(state: &PoolState, syncing_state: &mut PoolSyncingState) {
        let txg = syncing_state.syncing_txg.unwrap();

        let (object, next_block) = {
            let (phys, _) = syncing_state.pending_object.as_mut_pending();
            if phys.is_empty() {
                return;
            } else {
                (phys.header.object, phys.header.next_block)
            }
        };

        let (phys, callbacks) = mem::replace(
            &mut syncing_state.pending_object,
            PendingObjectState::new_pending(
                state.shared_state.guid,
                ObjectId::new(next_block),
                next_block,
                txg,
            ),
        )
        .unwrap_pending();

        assert_eq!(object, phys.header.object);

        Self::account_new_object(state, syncing_state, &phys);

        trace!("{:?}: writing {}", txg, phys);

        // write to object store and wake up waiters
        let shared_state = state.shared_state.clone();
        let guid = state.shared_state.guid;
        let cache = match *WRITES_INGEST_TO_ZETTACACHE {
            true => Some(state.zettacache.clone()),
            false => None,
        };
        measure!("Pool::initiate_flush_object_impl()").spawn(async move {
            if let Some(cache) = cache {
                measure!()
                    .fut(cache.insert_all(guid, &phys.blocks, InsertSource::Write))
                    .await;
            }

            measure!()
                .fut(phys.put(&shared_state.object_access, ObjectAccessOpType::TxgSyncPut))
                .await;
            for callback in callbacks {
                callback();
            }
        });
    }

    fn write_unordered_to_pending_object(
        state: &PoolState,
        syncing_state: &mut PoolSyncingState,
        size_limit_opt: Option<u32>,
        block_limit_opt: Option<BlockId>,
    ) {
        // If we're in the middle of resuming, we aren't building the pending object, so skip this
        if !syncing_state.pending_object.is_pending() {
            return;
        }

        let mut next_block = syncing_state.next_block();
        while let Some((block, (buf, callback))) = syncing_state.pending_unordered_writes.pop() {
            super_trace!(
                "found next {:?} in unordered pending writes; transferring to pending object",
                next_block
            );
            assert_eq!(block, next_block);
            let (phys, callbacks) = syncing_state.pending_object.as_mut_pending();
            phys.header.blocks_size += u32::try_from(buf.len()).unwrap();
            phys.blocks.insert(phys.header.next_block, buf);
            next_block = next_block.next();
            phys.header.next_block = next_block;
            callbacks.push(callback);
            if let Some(size_limit) = size_limit_opt {
                if phys.header.blocks_size >= size_limit {
                    Self::initiate_flush_object_impl(state, syncing_state);
                }
            }
            if let Some(block_limit) = block_limit_opt {
                if next_block == block_limit {
                    break;
                }
            }
        }
        Self::check_pending_flushes(state, syncing_state);
    }

    pub fn write_block(&self, block: BlockId, bytes: AlignedBytes, cb: WriteCallback) {
        self.state.with_syncing_state(|syncing_state| {
            // XXX change to return error
            assert!(syncing_state.syncing_txg.is_some());
            assert_ge!(block, syncing_state.next_block());

            super_trace!("inserting {:?} to unordered pending writes", block);
            syncing_state
                .pending_unordered_writes
                .insert(block, (bytes.as_bytes(), cb));

            Self::write_unordered_to_pending_object(
                &self.state,
                syncing_state,
                Some(TARGET_OBJECT_SIZE.as_u64().try_into().unwrap()),
                None,
            );
        });
    }

    async fn read_object_for_block(&self, block: BlockId) -> (Arc<DataObject>, GetMethod) {
        let mut object = self.state.object_block_map.block_to_object(block);
        let shared_state = self.state.shared_state.clone();
        loop {
            super_trace!("reading {:?} for {:?}", object, block);
            match DataObject::get(&shared_state.object_access, shared_state.guid, object).await {
                Ok(tuple) => return tuple,
                Err(e) => {
                    // We may have failed due to the object not existing, due to the
                    // object/block map changing out from under us, and then the object being
                    // freed.  If the OBM has changed, retry reading the new object.
                    let new_object = self.state.object_block_map.block_to_object(block);
                    if new_object != object {
                        debug!(
                            "got {} while reading {:?} for {:?}, retrying with new {:?}",
                            e, object, block, new_object
                        );
                        object = new_object;
                    } else {
                        panic!("got {} while reading {:?} for {:?}", e, object, block);
                    }
                }
            }
        }
    }

    async fn read_block_impl(&self, block: BlockId) -> Bytes {
        let mut object = self.state.object_block_map.block_to_object(block);
        let shared_state = self.state.shared_state.clone();
        loop {
            super_trace!("reading {:?} for {:?}", object, block);
            match DataObject::get_block(
                &shared_state.object_access,
                shared_state.guid,
                object,
                block,
            )
            .await
            {
                Ok(bytes) => return bytes,
                Err(e) => {
                    // We may have failed due to the object not existing, due to the
                    // object/block map changing out from under us, and then the object being
                    // freed.  If the OBM has changed, retry reading the new object.
                    let new_object = self.state.object_block_map.block_to_object(block);
                    if new_object != object {
                        debug!(
                            "got {} while reading {:?} for {:?}, retrying with new {:?}",
                            e, object, block, new_object
                        );
                        object = new_object;
                    } else {
                        panic!("got {} while reading {:?} for {:?}", e, object, block);
                    }
                }
            }
        }
    }

    pub async fn read_block(&self, block: BlockId, heal: bool) -> Bytes {
        // If we are in the middle of resuming, wait for that to complete before
        // processing this read.  This is needed because we may be reading from
        // a block that hasn't yet been added to the ObjectBlockMap.
        self.state.resuming.clone().recv().await.ok();

        let guid = self.state.shared_state.guid;
        let cache = &self.state.zettacache;
        if heal {
            // Using boxed() on this infrequently-called path reduces the size of the
            // RootConnectionState::read_block() future from 1200B->938B.
            return measure!()
                .fut(async move {
                    let bytes = self.read_block_impl(block).await;
                    cache.heal(guid, block, bytes.clone()).await;
                    bytes
                })
                .boxed()
                .await;
        }
        let object = self.state.object_block_map.block_to_object(block);
        // Look for this block in the object cache (memory).
        if let Some(bytes) = measure!("Pool::read_block() peak_block()")
            .fut(DataObject::peek_block(guid, object, block))
            .await
        {
            // If we are doing sibling block ingestion, then all of the object's blocks
            // (including this one) were recently added to the zettacache.  If not, we need to
            // add this block now.
            if !*SIBLING_BLOCKS_INGEST_TO_ZETTACACHE {
                match measure!().fut(cache.lookup(guid, block)).await {
                    LookupResponse::Present(..) => {
                        trace!("found {block:?} in object cache and zettacache")
                    }
                    LookupResponse::Absent(key) => {
                        cache.insert(key, bytes.clone(), InsertSource::Read).await;
                    }
                }
            }
            return bytes;
        }
        // This block was not in the object cache.  Check the zettacache (disk).
        match measure!().fut(cache.lookup(guid, block)).await {
            LookupResponse::Present(cached_bytes, _key) => cached_bytes.into(),
            LookupResponse::Absent(key) => {
                if *SIBLING_BLOCKS_INGEST_TO_ZETTACACHE {
                    // This block was not in the zettacache.  Read the entire object from the
                    // object store (network), and add all its blocks to the zettacache.
                    drop(key); // must not be held across insert_all()
                    let (data_object, method) = self.read_object_for_block(block).await;
                    if matches!(method, GetMethod::Loaded) {
                        cache
                            .insert_all(guid, &data_object.blocks, InsertSource::SpeculativeRead)
                            .await;
                    }
                    data_object
                        .blocks
                        .get(&block)
                        .with_context(|| format!("{block:?} not in {object:?}"))
                        .unwrap()
                        .clone()
                } else {
                    // This block was not in the zettacache.  Read it[*] from the object store
                    // (network), and add just this block to the zettacache.
                    // [*] (the DataObject layer may read the entire object or do a ranged read
                    // to get just this block out of the object)
                    let bytes = self.read_block_impl(block).await;
                    cache.insert(key, bytes.clone(), InsertSource::Read).await;
                    bytes
                }
            }
        }
    }

    pub fn free_blocks(&self, blocks: &[u64], sizes: &[u32]) {
        // the syncing_state is only held from the thread that owns the Pool
        // (i.e. this thread) and from end_txg(). It's not allowed to call this
        // function while in the middle of an end_txg(), so the lock must not be
        // held. XXX change this to return an error to the client
        assert_eq!(blocks.len(), sizes.len());
        self.state.with_syncing_state(|syncing_state| {
            for (&block, &size) in blocks.iter().zip(sizes.iter()) {
                syncing_state.log_free(
                    PendingFreesLogEntry {
                        block: BlockId(block),
                        size,
                    },
                    &self.state.object_block_map,
                );
            }
        })
    }

    async fn try_claim(&self, id: Uuid) -> OwnResult {
        let object_access = &self.state.shared_state.object_access;
        let guid = self.state.shared_state.guid;
        let start = Instant::now();
        let owner_res = PoolOwnerPhys::get(object_access, guid).await;
        let mut duration = Instant::now().duration_since(start);
        if let Ok(owner) = owner_res {
            info!("Owner found: {:?}", owner);
            if owner.owner == id {
                info!("Self owner found");
                if let Some(valid_after) = self.state.heartbeat_guard.as_ref().unwrap().valid_after
                {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(
                        valid_after.checked_add(*CLAIM_DURATION * 3).unwrap(),
                    ))
                    .await;
                }
                return handle_final_owner(object_access, guid, id).await;
            }
            let heartbeat_res = HeartbeatPhys::get(object_access, owner.owner).await;
            duration = Instant::now().duration_since(start);
            if let Ok(heartbeat) = heartbeat_res {
                info!("Heartbeat found: {:?}", heartbeat);
                /*
                 * We do this twice, because in the normal case we'll find an updated heartbeat
                 * within a couple seconds. In the case where there are unexpected s3 failures or
                 * network problems, we wait for the full duration.
                 */
                let short_duration = *HEARTBEAT_INTERVAL * 2;
                let long_duration = *LEASE_DURATION * 2 - short_duration;
                sleep(short_duration).await;
                if let Ok(new_heartbeat) = HeartbeatPhys::get(object_access, owner.owner).await {
                    if heartbeat.timestamp != new_heartbeat.timestamp {
                        return OwnResult::Failure(new_heartbeat);
                    }
                }
                sleep(long_duration).await;
                if let Ok(new_heartbeat) = HeartbeatPhys::get(object_access, owner.owner).await {
                    if heartbeat.timestamp != new_heartbeat.timestamp {
                        return OwnResult::Failure(new_heartbeat);
                    }
                }
                let time = Instant::now();
                if let Ok(new_owner) = PoolOwnerPhys::get(object_access, guid).await {
                    if new_owner.owner != owner.owner {
                        return OwnResult::Failure(
                            HeartbeatPhys::get(object_access, new_owner.owner)
                                .await
                                .unwrap(),
                        );
                    }
                }
                duration = Instant::now().duration_since(time);
            }
        }

        if duration > *CLAIM_DURATION {
            return OwnResult::Retry;
        }

        let owner = PoolOwnerPhys {
            id: guid,
            owner: id,
        };

        let put_result = owner
            .put_timeout(object_access, Some(*CLAIM_DURATION - duration))
            .await;

        if let Err(OAError::TimeoutError(_)) = put_result {
            return OwnResult::Retry;
        }
        sleep(*CLAIM_DURATION * 3).await;

        handle_final_owner(object_access, guid, id).await
    }

    pub async fn claim(&mut self, id: Uuid) -> Result<(), PoolOpenError> {
        if self.state.shared_state.object_access.readonly() {
            return Ok(());
        }
        loop {
            match self.try_claim(id).await {
                OwnResult::Success => {
                    return Ok(());
                }
                OwnResult::Failure(heartbeat) => {
                    return Err(PoolOpenError::Mmp(heartbeat.hostname));
                }
                OwnResult::Retry => {
                    continue;
                }
            }
        }
    }

    async fn unclaim(self) {
        if !self.state.shared_state.object_access.readonly() {
            PoolOwnerPhys::delete(
                &self.state.shared_state.object_access,
                self.state.shared_state.guid,
            )
            .await;
        }
    }

    pub async fn close(self, destroy: bool) {
        if destroy {
            // Kick off a task to destroy the pool's zettaobjects.
            info!(
                "Pool marked for destroying {}",
                self.state.shared_state.guid
            );
            pool_destroy::destroy_pool(
                self.state.shared_state.object_access.clone(),
                self.state.shared_state.guid,
                self.state.object_block_map.len() as u64,
            )
            .await;
        }

        self.unclaim().await;
    }

    pub fn enable_feature(&self, feature_name: &str) {
        let feature = crate::features::get_feature(feature_name).unwrap_or_else(|| {
            panic!(
                "Unknown feature {} requested by kernel; update agent",
                feature_name
            )
        });
        self.state.with_syncing_state(|syncing_state| {
            if let hash_map::Entry::Vacant(e) = syncing_state.features.entry(feature) {
                e.insert(0);
            }
        });
    }
}

async fn recover_list(
    state: &Arc<PoolState>,
    shared_state: &Arc<PoolSharedState>,
) -> BTreeMap<ObjectId, DataObject> {
    assert!(shared_state.object_access.supports_list_after());

    let begin = Instant::now();
    let last_obj = state.object_block_map.last_object();
    let list_stream = FuturesUnordered::new();
    for prefix in DataObject::prefixes(shared_state.guid) {
        let shared_state = shared_state.clone();
        list_stream.push(async move {
            let start_after = format!("{}{}", prefix, last_obj);
            shared_state
                .object_access
                .try_list_after(prefix, true, start_after)
                .unwrap()
                .collect::<Vec<String>>()
                .await
        });
    }
    let recovered = list_stream
        .flat_map(|vec| {
            let sub_stream = FuturesUnordered::new();
            for key in vec {
                let shared_state = shared_state.clone();
                sub_stream.push(future::ready(async move {
                    DataObject::get_from_key(
                        &shared_state.object_access,
                        key,
                        ObjectAccessOpType::ReadsGet,
                    )
                    .await
                }));
            }
            sub_stream
        })
        .buffer_unordered(50)
        .fold(BTreeMap::new(), |mut map, data_res| async move {
            let data = data_res.unwrap();
            debug!(
                "resume: found {:?}, next {:?}",
                data.header.object, data.header.next_block
            );
            assert_eq!(data.header.guid, shared_state.guid);
            map.insert(data.header.object, data);
            map
        })
        .await;
    info!(
        "resume: listed and read {} objects in {}ms",
        recovered.len(),
        begin.elapsed().as_millis()
    );
    recovered
}

async fn handle_final_owner(
    object_access: &Arc<ObjectAccess>,
    guid: PoolGuid,
    expected: Uuid,
) -> OwnResult {
    let final_owner = PoolOwnerPhys::get(object_access, guid).await.unwrap();
    if final_owner.owner != expected {
        return OwnResult::Failure(
            HeartbeatPhys::get(object_access, final_owner.owner)
                .await
                .unwrap_or(HeartbeatPhys {
                    timestamp: SystemTime::now(),
                    hostname: "unknown".to_string(),
                    lease_duration: *LEASE_DURATION,
                    id: final_owner.owner,
                }),
        );
    }
    OwnResult::Success
}

//
// Following routines deal with reclaiming free space
//

fn log_new_sizes(txg: Txg, reclaim_log: &mut ReclaimLog, rewritten_object_sizes: Vec<ObjectSize>) {
    let object_size_log = &mut reclaim_log.object_size_log;

    for object_size in rewritten_object_sizes {
        // log to on-disk size
        trace!("logging {:?}", object_size);
        object_size_log.append(txg, ObjectSizeLogEntry::Exists(object_size));
    }
}

fn log_deleted_objects(
    state: Arc<PoolState>,
    syncing_state: &mut PoolSyncingState,
    deleted_objects: Vec<ObjectId>,
) {
    let txg = syncing_state.syncing_txg.unwrap();
    for &object in &deleted_objects {
        syncing_state
            .storage_object_log
            .append(txg, StorageObjectLogEntry::Free { object });
        state.object_block_map.remove(object);
        syncing_state
            .get_pending_frees_log_for_obj(object)
            .object_size_log
            .append(txg, ObjectSizeLogEntry::Freed { object });
        syncing_state.stats.objects_count -= 1;
    }
    syncing_state.object_deleter.delete(deleted_objects);
}

/// builds a new pending frees log based off the remainder from reclaiming
async fn build_new_frees<'a, I>(
    txg: Txg,
    reclaim_log: &mut ReclaimLog,
    remaining_frees: I,
    remainder: ObjectBasedLogRemainder,
) where
    I: IntoIterator<Item = &'a PendingFreesLogEntry>,
{
    let begin = Instant::now();

    let log = &mut reclaim_log.pending_frees_log;

    // We need to call .iter_remainder() before .clear(), otherwise we'd be
    // iterating the new, empty generation.
    let stream = log.iter_remainder(txg, remainder).await;
    log.clear(txg).await;

    // Note: We recalculate the pending free bytes here to validate against the free log stats
    let mut count: u64 = 0;
    let mut bytes: u64 = 0;
    for ent in remaining_frees {
        log.append(txg, *ent);
        count += 1;
        bytes += u64::from(ent.size);
    }
    stream
        .for_each(|ent| {
            log.append(txg, ent);
            count += 1;
            bytes += u64::from(ent.size);
            future::ready(())
        })
        .await;
    // Note: the caller (end_txg_cb()) is about to call flush(), but doing it here ensures that
    // the time to PUT these objects is accounted for in the info!() below.
    log.flush(txg).await;

    info!(
        "reclaim: {:?} transferred {} freed blocks ({}MiB) in {}ms",
        txg,
        count,
        reclaim_log.pending_free_bytes / ONE_MIB,
        begin.elapsed().as_millis()
    );
    assert_eq!(bytes, reclaim_log.pending_free_bytes);
}

async fn get_object_sizes(
    object_size_log_stream: impl Stream<Item = ObjectSizeLogEntry>,
) -> BTreeSet<ObjectSize> {
    let mut object_sizes = BTreeSet::new();
    let begin = Instant::now();
    object_size_log_stream
        .for_each(|ent| {
            match ent {
                ObjectSizeLogEntry::Exists(object_size) => {
                    // Overwrite existing value, if any.  We have to explicitly remove it using
                    // the ObjectId so that we find any entry that matches this ObjectId (even
                    // with a different num_blocks/bytes).
                    object_sizes.remove(&object_size.object);
                    object_sizes.insert(object_size);
                }
                ObjectSizeLogEntry::Freed { object } => {
                    let removed = object_sizes.remove(&object);
                    // value must already exist
                    assert!(removed);
                }
            }
            future::ready(())
        })
        .await;
    info!(
        "reclaim: loaded sizes for {} objects in {}ms",
        object_sizes.len(),
        begin.elapsed().as_millis()
    );
    object_sizes
}

/// returns (free_bytes, map), where free_bytes is the number of free bytes in the log, and map
/// lists the frees associated with each object.
async fn get_frees_per_obj(
    state: &PoolState,
    pending_frees_log_stream: impl Stream<Item = PendingFreesLogEntry>,
) -> (u64, HashMap<ObjectId, Vec<PendingFreesLogEntry>>) {
    // XXX The Vecs will grow by doubling, thus wasting ~1/4 of the memory used by it.  It would
    // be better if we gathered the BlockID's into a single big Vec with the exact required size,
    // then in-place sort, and then have this map to a slice of the one big Vec.
    let mut frees_per_obj: HashMap<ObjectId, Vec<PendingFreesLogEntry>> = HashMap::new();
    let mut count: u64 = 0;
    let mut freed_bytes: u64 = 0;
    let begin = Instant::now();
    pending_frees_log_stream
        .for_each(|ent| {
            let obj = state.object_block_map.block_to_object(ent.block);
            // XXX change to debug-only assert?
            assert!(!frees_per_obj.entry(obj).or_default().contains(&ent));
            frees_per_obj.entry(obj).or_default().push(ent);
            count += 1;
            freed_bytes += u64::from(ent.size);
            future::ready(())
        })
        .await;
    info!(
        "reclaim: loaded {} freed blocks in {}ms",
        count,
        begin.elapsed().as_millis()
    );
    (freed_bytes, frees_per_obj)
}

async fn reclaim_frees_object(
    state: &PoolState,
    objects: Vec<(ObjectSize, Vec<PendingFreesLogEntry>)>,
) -> ObjectSize {
    // objects should be sorted
    assert!(objects.windows(2).all(|w| w[0].0.object < w[1].0.object));

    let first_object = objects.first().unwrap().0.object;
    let last_object = objects.last().unwrap().0.object;

    // Note that the .reduce() below can't completely determine the next_block because if we skip
    // the GET (because this object doesn't have any non-freed blocks), it won't be visited by
    // the .reduce() when the filter_map() below returns None.
    let next_block = state.object_block_map.object_to_next_block(last_object);
    trace!(
        "reclaim: consolidating {} objects into {:?} to free {} blocks (last {:?} next {:?})",
        objects.len(),
        first_object,
        objects.iter().map(|(_, frees)| frees.len()).sum::<usize>(),
        last_object,
        next_block,
    );

    let futures = objects.into_iter().filter_map(move |(object_size, frees)| {
        let object = object_size.object;
        let min_block = object.as_min_block();
        let next_block = state.object_block_map.object_to_next_block(object);

        if object != first_object && object_size.num_blocks == 0 {
            trace!(
                "reclaim: moving 0 blocks from {:?} (BlockID[{},{})) because all {} blocks were freed",
                object,
                min_block,
                next_block,
                frees.len(),
            );
            assert_eq!(object_size.num_bytes, 0);
            return None;
        }

        let shared_state = state.shared_state.clone();
        Some(async move {
            // Bypass object cache so that it isn't added, so that when we overwrite it with
            // put(), we don't need to copy the data into the cache to invalidate.
            let mut phys =
                DataObject::get_uncached(&shared_state.object_access, shared_state.guid, object, ObjectAccessOpType::ReclaimGet)
                    .await
                    .unwrap();

            for ent in frees {
                // If we crashed in the middle of this operation last time, the block may already
                // have been removed (and the object rewritten), however the stats were not yet
                // updated (since that happens as part of txg_end, atomically with the updates to
                // the PendingFreesLog).  In this case we ignore the fact that it isn't present,
                // but count this block as removed for stats purposes.
                if let Some(v) = phys.blocks.remove(&ent.block) {
                    assert_eq!(u32::try_from(v.len()).unwrap(), ent.size);
                    phys.header.blocks_size -= ent.size;
                }
            }

            // The object could have been rewritten as part of a previous reclaim that we crashed
            // in the middle of.  In that case, the object may have additional blocks which we do
            // not expect (past next_block).  However, the expected size (new_object_size) must
            // match the size of the blocks within the expected range (up to next_block).
            // Additionally, any blocks outside the expected range are also represented in their
            // expected objects.  So, we can correctly remove them from this object, undoing the
            // previous, uncommitted consolidation.  Therefore, if the expected size is zero, we
            // can remove this object without reading it because it doesn't have any required
            // blocks.  That happens above, where we `return None`.
            if phys.header.next_block != next_block {
                debug!("reclaim: {:?} expected next {:?}, found next {:?}, trimming uncommitted consolidation",
                    object, next_block, phys.header.next_block);
                phys
                    .blocks
                    .retain(|block, _| block < &next_block);

                assert_ge!(phys.header.blocks_size, object_size.num_bytes);
                phys.header.blocks_size = object_size.num_bytes;

                assert_ge!(phys.header.next_block, next_block);
                phys.header.next_block = next_block;
            }
            assert_eq!(phys.header.blocks_size, phys.calculate_blocks_size());
            assert_eq!(phys.header.blocks_size, object_size.num_bytes);
            assert_eq!(phys.blocks_len(), object_size.num_blocks);

            phys
        })
    });
    let mut new_phys = with_alloctag("reclaim_frees_object() buffered()", || stream::iter(futures)
        .buffered(*RECLAIM_ONE_BUFFERED))
        .reduce(|mut a, mut b| async move {
            assert_eq!(a.header.guid, b.header.guid);
            trace!(
                "reclaim: moving {} blocks from {:?} (TXG[{},{}] next {:?}) to {:?} (TXG[{},{}] next {:?})",
                b.blocks_len(),
                b.header.object,
                b.header.min_txg.0,
                b.header.max_txg.0,
                b.header.next_block,
                a.header.object,
                a.header.min_txg.0,
                a.header.max_txg.0,
                a.header.next_block,
            );
            a.header.object = min(a.header.object, b.header.object);
            a.header.min_txg = min(a.header.min_txg, b.header.min_txg);
            a.header.max_txg = max(a.header.max_txg, b.header.max_txg);
            a.header.next_block = max(a.header.next_block, b.header.next_block);
            let mut already_moved: u32 = 0;
            for (k, v) in b.blocks.drain() {
                let len = u32::try_from(v.len()).unwrap();
                match a.blocks.insert(k, v) {
                    Some(old_bytes) => {
                        // May have already been transferred in a previous job
                        // during which we crashed before updating the metadata.
                        assert_eq!(old_bytes, a.block(k));
                        already_moved += 1;
                    }
                    None => {
                        a.header.blocks_size += len;
                    }
                }
            }
            if already_moved > 0 {
                debug!(
                    "reclaim: while moving blocks from {:?} to {:?} found {} blocks already moved",
                    b.header.object, a.header.object, already_moved
                );
            }
            a
        })
        .await
        .unwrap();

    // Fold in the next_block info which includes the skipped objects.
    assert_eq!(new_phys.header.object, first_object);
    assert_le!(new_phys.header.next_block, next_block);
    new_phys.header.next_block = next_block;

    // XXX would be nice to skip this if we didn't actually make any change
    // (because we already did it all before crashing)
    trace!("reclaim: rewriting {}", new_phys);
    new_phys
        .put(
            &state.shared_state.object_access,
            ObjectAccessOpType::ReclaimPut,
        )
        .await;

    (&new_phys).into()
}

/// Split the pending frees content across two logs
async fn split_pending_frees_log(
    txg: Txg,
    state: &Arc<PoolState>,
    syncing_state: &mut PoolSyncingState,
    original_id: ReclaimLogId,
    sibling_id: ReclaimLogId,
) {
    let reclaim_log = syncing_state.get_pending_frees_log(original_id);
    reclaim_log.pending_frees_log.flush(txg).await;
    let pending_frees_log_stream = reclaim_log.pending_frees_log.iterate();

    // Clear original log and pending_free_bytes since they are going to be rebuilt
    assert!(!reclaim_log.reclaim_busy);
    reclaim_log.pending_frees_log.clear(txg).await;
    reclaim_log.pending_free_bytes = 0;

    pending_frees_log_stream
        .for_each(|ent| {
            let id = syncing_state.get_log_id(state.object_block_map.block_to_object(ent.block));
            assert!(id == original_id || id == sibling_id);
            let this_reclaim_log = syncing_state.get_pending_frees_log(id);
            this_reclaim_log.pending_frees_log.append(txg, ent);
            this_reclaim_log.pending_free_bytes += u64::from(ent.size);
            future::ready(())
        })
        .await;

    for id in &[original_id, sibling_id] {
        let log = syncing_state.get_pending_frees_log(*id);
        log.pending_frees_log.flush(txg).await;
        debug!(
            "reclaim: {:?} split pending frees {:?} has {} entries and {} MiB",
            txg,
            id,
            log.pending_frees_log.num_entries,
            log.pending_free_bytes / ONE_MIB
        );
    }
}

/// Split the object sizes content across two logs
async fn split_object_sizes_log(
    txg: Txg,
    syncing_state: &mut PoolSyncingState,
    original_id: ReclaimLogId,
    sibling_id: ReclaimLogId,
) {
    let reclaim_log = &mut syncing_state.get_pending_frees_log(original_id);
    reclaim_log.object_size_log.flush(txg).await;
    let object_size_log_stream = reclaim_log.object_size_log.iterate();

    // Clear previous log since it's going to be rebuilt
    reclaim_log.object_size_log.clear(txg).await;

    object_size_log_stream
        .for_each(|ent| {
            let object = match ent {
                ObjectSizeLogEntry::Exists(object_size) => object_size.object,
                ObjectSizeLogEntry::Freed { object } => object,
            };
            let id = syncing_state.get_log_id(object);
            assert!(id == original_id || id == sibling_id);
            syncing_state
                .get_pending_frees_log(id)
                .object_size_log
                .append(txg, ent);
            future::ready(())
        })
        .await;

    for id in &[original_id, sibling_id] {
        let log = syncing_state.get_pending_frees_log(*id);
        log.object_size_log.flush(txg).await;
        debug!(
            "reclaim: {:?} split object sizes {:?} has {} entries",
            txg, id, log.object_size_log.num_entries
        );
    }
}

/// Split logs that are getting full
async fn try_split_reclaim_logs(state: Arc<PoolState>, syncing_state: &mut PoolSyncingState) {
    let reclaim_log = match syncing_state
        .reclaim_info
        .reclaim_logs
        .iter_mut()
        // Filter logs that are not candidates for splitting
        .filter(|a| {
            !a.reclaim_busy
                && a.num_bits != RECLAIM_TABLE_MAX_BITS
                && a.pending_frees_log.num_entries >= *RECLAIM_LOG_ENTRIES_LIMIT
        })
        // Locate the largest log remaining (if any)
        .reduce(|a, b| {
            if a.pending_frees_log.num_entries >= b.pending_frees_log.num_entries {
                a
            } else {
                b
            }
        }) {
        Some(reclaim_log) => reclaim_log,
        None => return,
    };

    let begin = Instant::now();
    let log_bits = reclaim_log.num_bits;
    let log_prefix = reclaim_log.prefix;

    info!(
        "reclaim: splitting pending frees log {:?} with {} entries (bits = {}, limit = {})",
        reclaim_log.id,
        reclaim_log.pending_frees_log.num_entries,
        log_bits,
        *RECLAIM_LOG_ENTRIES_LIMIT
    );

    // Expand log's prefix and bit count
    reclaim_log.num_bits += 1;
    reclaim_log.prefix <<= 1;

    let indirect_table = &mut syncing_state.reclaim_info.indirect_table;
    let table_bits = indirect_table.table_bits;

    // Grow the indirect table if this log has no more siblings in the table
    if table_bits == log_bits {
        indirect_table.grow_table();
    }

    // Create the new log files for the split
    let pool_guid = state.shared_state.guid;
    let log_id = reclaim_log.id;
    let new_log_id =
        ReclaimLogId(u16::try_from(syncing_state.reclaim_info.reclaim_logs.len()).unwrap());
    let new_log = ReclaimLog {
        reclaim_busy: false,
        num_bits: log_bits + 1,
        prefix: (log_prefix << 1) | 1,
        id: new_log_id,
        pending_frees_log: ObjectBasedLog::create(
            state.shared_state.clone(),
            &format!("zfs/{}/PendingFreesLog/{}", pool_guid, new_log_id),
        ),
        pending_free_bytes: 0,
        object_size_log: ObjectBasedLog::create(
            state.shared_state.clone(),
            &format!("zfs/{}/ObjectSizeLog/{}", pool_guid, new_log_id),
        ),
    };
    syncing_state.reclaim_info.reclaim_logs.push(new_log);

    // Update sibling indices in the table to point to the new logs (used by get_log_id below)
    indirect_table.update_siblings(log_bits + 1, (log_prefix << 1) | 1, new_log_id);

    // log the latest table
    if indirect_table.log_ids.len() <= 256 {
        debug!("{}", syncing_state.reclaim_info.indirect_table);
    }

    // read previous log content and distribute it across original and new
    let txg = syncing_state.syncing_txg.unwrap();
    split_pending_frees_log(txg, &state, syncing_state, log_id, new_log_id).await;
    split_object_sizes_log(txg, syncing_state, log_id, new_log_id).await;

    // log the distribution of current logs
    for p in syncing_state.reclaim_info.reclaim_logs.iter() {
        debug!(
            "reclaim: log {:?}: {} entries",
            p.id, p.pending_frees_log.num_entries
        )
    }

    info!(
        "reclaim: {:?} split of reclaim logs took {}ms",
        txg,
        begin.elapsed().as_millis()
    );
}

/// reclaim free blocks from one of our pending-free logs processes the log with the most space
/// freed If there is a checkpoint, this is a no-nop.
fn try_reclaim_frees(state: Arc<PoolState>, syncing_state: &mut PoolSyncingState) {
    if syncing_state.reclaim_done.is_some() || syncing_state.checkpoint_txg.is_some() {
        return;
    }

    if syncing_state.stats.pending_frees_bytes
        < FREE_HIGHWATER_PCT.apply(syncing_state.stats.blocks_bytes)
        || syncing_state.stats.pending_frees_count < *FREE_MIN_BLOCKS
    {
        return;
    }
    info!(
        "reclaim: {:?} starting; pending_frees_bytes={}Mib, blocks_bytes={}Mib, {} free blocks pending",
        syncing_state.syncing_txg.unwrap(),
        syncing_state.stats.pending_frees_bytes / ONE_MIB,
        syncing_state.stats.blocks_bytes / ONE_MIB,
        syncing_state.stats.pending_frees_count
    );

    // Note: the object size stream may or may not include entries added this txg.  Fortunately,
    // the frees stream can't have any frees within object created this txg, so this is not a
    // problem.

    // Load the log with the most space freed
    let best_reclaim_log = syncing_state
        .reclaim_info
        .reclaim_logs
        .iter_mut()
        .reduce(|a, b| {
            if a.pending_free_bytes >= b.pending_free_bytes {
                a
            } else {
                b
            }
        })
        .unwrap();

    info!(
        "reclaim: using {:?} with {} free entries, {}MiB free bytes",
        best_reclaim_log.id,
        best_reclaim_log.pending_frees_log.num_entries,
        best_reclaim_log.pending_free_bytes / ONE_MIB
    );

    let best_log = best_reclaim_log.id;
    let (pending_frees_log_stream, frees_remainder) =
        best_reclaim_log.pending_frees_log.iter_most();
    let (object_size_log_stream, sizes_remainder) = best_reclaim_log.object_size_log.iter_most();
    // Note: The split code will shrink the log entries and reset the log's pending_free_bytes.
    // The *_remainders streams implicitly depend on the *_logs not being reset (only appended
    // to). Likewise, the pending_free_bytes is assumed not to shrink during the reclaim (can
    // only increase). Tag this log so that the split code will choose another candidate.
    best_reclaim_log.reclaim_busy = true;

    let (sender, receiver) = oneshot::channel();
    syncing_state.reclaim_done = Some(receiver);

    measure!("try_reclaim_frees()").spawn(async move {
        // load pending frees
        let (freed_bytes, mut frees_per_object) =
            get_frees_per_obj(&state, pending_frees_log_stream).await;

        let required_free_bytes = FREE_LOWWATER_PCT.apply(freed_bytes);

        // sort objects by number of free blocks
        // XXX should be based on free space (bytes)?  And perhaps objects that will be entirely
        // freed should always be processed?
        // XXX we want to maximize bytes freed per unit time. network bandwidth is the constraint
        // on time, so bytes freed per bytes read+written would be a good metric. (or just read
        // or just written, if we knew which direction was the performance constraint; we are
        // reading more than writing, but if caching is effective then other processes may be
        // doing more writing than reading)
        let mut objects_by_frees: BTreeSet<(usize, ObjectId)> = BTreeSet::new();
        for (obj, hs) in frees_per_object.iter() {
            // MAX-len because we want to sort by which has the most to free, (high to low) and
            // then by object ID (low to high) because we consolidate forward
            objects_by_frees.insert((usize::MAX - hs.len(), *obj));
        }

        // load object sizes
        let object_sizes = get_object_sizes(object_size_log_stream).await;

        let begin = Instant::now();

        let mut join_handles = Vec::new();
        let mut freed_blocks_count: u64 = 0;
        let mut freed_blocks_bytes: u64 = 0;
        let mut rewritten_object_sizes: Vec<ObjectSize> = Vec::new();
        let mut deleted_objects: Vec<ObjectId> = Vec::new();
        let mut writing: HashSet<ObjectId> = HashSet::new();
        let outstanding = Arc::new(tokio::sync::Semaphore::new(*RECLAIM_QUEUE_DEPTH));
        for (_, object) in objects_by_frees {
            if !frees_per_object.contains_key(&object) {
                // this object is being removed by a multi-object consolidation
                continue;
            }
            // XXX limit amount of outstanding get/put requests?
            let mut objects_to_consolidate: Vec<(ObjectSize, Vec<PendingFreesLogEntry>)> =
                Vec::new();
            let mut new_size: u32 = 0;
            assert!(object_sizes.contains(&object));
            let mut first = true;
            let (_min_object, max_object) = PoolSyncingState::get_log_range(object);
            for later_object_size in object_sizes.range(object..max_object) {
                let later_object = later_object_size.object;
                let empty_vec = Vec::new();
                let later_object_frees = frees_per_object.get(&later_object).unwrap_or(&empty_vec);
                let later_bytes_freed: u32 = later_object_frees.iter().map(|e| e.size).sum();
                let later_blocks_freed = u32::try_from(later_object_frees.len()).unwrap();
                let later_object_new_size = ObjectSize {
                    object: later_object,
                    num_blocks: later_object_size.num_blocks - later_blocks_freed,
                    num_bytes: later_object_size.num_bytes - later_bytes_freed,
                };
                if first {
                    assert_eq!(object, later_object);
                    assert!(!writing.contains(&later_object));
                    first = false;
                } else {
                    // If we run into an object that we're already writing, we
                    // can't consolidate with it.
                    if writing.contains(&later_object) {
                        break;
                    }
                    if new_size + later_object_new_size.num_bytes
                        > TARGET_OBJECT_SIZE.as_u64().try_into().unwrap()
                    {
                        break;
                    }
                }
                new_size += later_object_new_size.num_bytes;
                let frees = frees_per_object.remove(&later_object).unwrap_or_default();
                freed_blocks_count += u64::from(later_blocks_freed);
                freed_blocks_bytes += u64::from(later_bytes_freed);
                objects_to_consolidate.push((later_object_new_size, frees));
            }
            // XXX look for earlier objects too?

            // Must include at least the target object
            assert_eq!(objects_to_consolidate[0].0.object, object);

            writing.insert(object);

            // all but the first object need to be deleted by syncing context
            for (later_object_size, _) in objects_to_consolidate.iter().skip(1) {
                //complete.rewritten_object_sizes.push((*obj, 0));
                deleted_objects.push(later_object_size.object);
            }
            // Note: we could calculate the new object's size here as well, but that would be
            // based on the object_sizes map/log, which may have inaccuracies if we crashed
            // during reclaim.  Instead we calculate the size based on the object contents, and
            // return it from the spawned task.

            // Reclaim_frees_object reads all its objects in parallel, up to
            // RECLAIM_ONE_BUFFERED.
            let num_permits: u32 = min(*RECLAIM_ONE_BUFFERED, objects_to_consolidate.len())
                .try_into()
                .unwrap();
            let permit = outstanding.clone().acquire_many_owned(num_permits).await;
            let state = state.clone();
            join_handles.push(measure!("reclaim_frees_object").spawn(async move {
                let _permit = permit; // force permit to be moved & dropped in the task
                reclaim_frees_object(&state, objects_to_consolidate).await
            }));
            if freed_blocks_bytes > required_free_bytes {
                break;
            }
        }
        let num_handles = join_handles.len();
        for join_handle in join_handles {
            rewritten_object_sizes.push(join_handle.await.unwrap());
        }

        info!(
            "reclaim: rewrote {} objects in {:.1}sec, freeing {} MiB from {} blocks ({:.1}MiB/s)",
            num_handles,
            begin.elapsed().as_secs_f64(),
            freed_blocks_bytes / ONE_MIB,
            freed_blocks_count,
            ((freed_blocks_bytes as f64 / ONE_MIB as f64) / begin.elapsed().as_secs_f64()),
        );

        let r = sender.send(Box::new(move |syncing_state| {
            Box::pin(async move {
                syncing_state.stats.blocks_count -= freed_blocks_count;
                syncing_state.stats.blocks_bytes -= freed_blocks_bytes;
                syncing_state.stats.pending_frees_count -= freed_blocks_count;
                syncing_state.stats.pending_frees_bytes -= freed_blocks_bytes;
                syncing_state
                    .get_pending_frees_log(best_log)
                    .pending_free_bytes -= freed_blocks_bytes;

                let txg = syncing_state.syncing_txg.unwrap();
                let remaining_frees = frees_per_object.values().flatten();
                build_new_frees(
                    txg,
                    syncing_state.get_pending_frees_log(best_log),
                    remaining_frees,
                    frees_remainder,
                )
                .await;

                log_deleted_objects(state, syncing_state, deleted_objects);
                try_condense_object_sizes(
                    txg,
                    syncing_state.get_pending_frees_log(best_log),
                    object_sizes,
                    sizes_remainder,
                )
                .await;

                log_new_sizes(
                    txg,
                    syncing_state.get_pending_frees_log(best_log),
                    rewritten_object_sizes,
                );

                syncing_state.reclaim_done = None;
                syncing_state.get_pending_frees_log(best_log).reclaim_busy = false;
            })
        }));
        assert!(r.is_ok()); // can not use .unwrap() because the type is not Debug
    });
}

//
// following routines deal with condensing other ObjectBasedLogs
//

async fn try_condense_object_log(state: Arc<PoolState>, syncing_state: &mut PoolSyncingState) {
    // XXX change this to be based on bytes, once those stats are working?
    let len = state.object_block_map.len();
    if syncing_state.storage_object_log.num_chunks
        < (*LOG_CONDENSE_MIN_CHUNKS
            + *LOG_CONDENSE_MULTIPLE * (len + *ENTRIES_PER_OBJECT) / *ENTRIES_PER_OBJECT)
            as u64
    {
        return;
    }
    let txg = syncing_state.syncing_txg.unwrap();
    info!(
        "{:?} storage_object_log condense: starting; objects={} entries={} len={}",
        txg,
        syncing_state.storage_object_log.num_chunks,
        syncing_state.storage_object_log.num_entries,
        len
    );

    let begin = Instant::now();
    syncing_state.storage_object_log.clear(txg).await;
    state.object_block_map.for_each(|object| {
        syncing_state
            .storage_object_log
            .append(txg, StorageObjectLogEntry::Alloc { object })
    });
    // Note: the caller (end_txg_cb()) is about to call flush(), but doing it here ensures that
    // the time to PUT these objects is accounted for in the info!() below.
    syncing_state.storage_object_log.flush(txg).await;

    info!(
        "{:?} storage_object_log condense: wrote {} entries to {} objects in {}ms",
        txg,
        syncing_state.storage_object_log.num_entries,
        syncing_state.storage_object_log.num_chunks,
        begin.elapsed().as_millis()
    );
}

async fn try_condense_object_sizes(
    txg: Txg,
    reclaim_log: &mut ReclaimLog,
    object_sizes: BTreeSet<ObjectSize>,
    remainder: ObjectBasedLogRemainder,
) {
    let object_size_log = &mut reclaim_log.object_size_log;

    // XXX change this to be based on bytes, once those stats are working?
    let len = object_sizes.len();
    if object_size_log.num_chunks
        < (*LOG_CONDENSE_MIN_CHUNKS
            + *LOG_CONDENSE_MULTIPLE * (len + *ENTRIES_PER_OBJECT) / *ENTRIES_PER_OBJECT)
            as u64
    {
        return;
    }

    info!(
        "{:?} object_size_log condense: starting; objects={} entries={} len={}",
        txg, object_size_log.num_chunks, object_size_log.num_entries, len
    );

    let begin = Instant::now();
    // We need to call .iterate_after() before .clear(), otherwise we'd be iterating the new,
    // empty generation.
    let stream = object_size_log.iter_remainder(txg, remainder).await;
    object_size_log.clear(txg).await;
    for object_size in object_sizes {
        object_size_log.append(txg, ObjectSizeLogEntry::Exists(object_size));
    }

    stream
        .for_each(|ent| {
            object_size_log.append(txg, ent);
            future::ready(())
        })
        .await;
    // Note: the caller (end_txg_cb()) is about to call flush(), but doing it here ensures that
    // the time to PUT these objects is accounted for in the info!() below.
    object_size_log.flush(txg).await;

    info!(
        "{:?} object_size_log condense: wrote {} entries to {} objects in {}ms",
        txg,
        object_size_log.num_entries,
        object_size_log.num_chunks,
        begin.elapsed().as_millis()
    );
}

/// If there is a checkpoint, this is a no-op.
fn clean_metadata(
    state: Arc<PoolState>,
    syncing_state: &mut PoolSyncingState,
) -> Option<JoinHandle<()>> {
    let oldest_valid_txg = match syncing_state
        .syncing_txg
        .unwrap()
        .checked_sub(*METADATA_RETENTION_TXGS)
    {
        Some(txg) => txg,
        None => {
            return None;
        }
    };
    if syncing_state.checkpoint_txg.is_some() {
        return None;
    }
    Some(measure!("clean_metadata()").spawn(async move {
        let ub = match UberblockPhys::get(
            &state.shared_state.object_access,
            state.shared_state.guid,
            oldest_valid_txg,
        )
        .await
        {
            Ok(ub) => ub,
            Err(_) => {
                return;
            }
        };

        ub.storage_object_log
            .cleanup_older_generations(&state.shared_state.object_access)
            .await;
        for log_phys in ub.reclaim_info.reclaim_logs.iter() {
            /*
             * XXX We shouldn't run all of these serially in every TXG. Not only would it be slow
             * to wait for the necessary list operations, but we pay per request to s3.
             *
             * Instead, we should store in memory the lowest generation of a given log. We can
             * quickly compare that value to the ObjectBasedLogPhys here, and determine whether
             * any cleanup is necessary. We need to populate the list of lowest generations when
             * we import the pool, but that is cheap compared to getting the full list every txg.
             */
            log_phys
                .pending_frees_log
                .cleanup_older_generations(&state.shared_state.object_access)
                .await;
            log_phys
                .object_size_log
                .cleanup_older_generations(&state.shared_state.object_access)
                .await;
        }

        UberblockPhys::cleanup_older_uberblocks(&state.shared_state.object_access, ub).await;
    }))
}
