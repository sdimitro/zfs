//! This module provides common zettacache stat structures that are collected by
//! the **zettacache** runtime and consumed by **zcache** subcommands.
//! These structures on the zettacache side are serialized and then deserialized
//! by the zettacache subcommands.

//
// Note: For zettacache side (stats collection) all stat values need to be Atomic64
// values. However, atomicity is not required in the consumer. These Atomic64 could
// be avoided in the consumer by defining matching structs built on u64 values and
// informing `serde_json::from_str()` of the expected types for deserialization.
//
// The simplicity of having only one set of structs seems to outweigh the cost of
// having to maintain two sets of structs and the minor inconvenience of having to
// dereference the Atomic values to access them.
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]

use std::fmt::Display;
use std::fmt::Formatter;
use std::ops::AddAssign;
use std::ops::Sub;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use arr_macro::arr;
use enum_map::Enum;
use enum_map::EnumMap;
use num_traits::cast::ToPrimitive;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::nice_number_count;
use crate::nice_number_time;
use crate::nice_p2size;
use crate::write_stdout;

/// The zettacache disk I/O types that are collected and displayed for each disk.
#[derive(Debug, Enum, Copy, Clone, Serialize, Deserialize)]
pub enum DiskIoType {
    ReadDataForLookup,
    ReadIndexForLookup,
    WriteDataForInsert,
    MaintenanceRead,
    MaintenanceWrite,
    // Add any new I/O types here
}

impl Display for DiskIoType {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

//
// The collected stat values can be one of StatCount, StatBytes, or StatLatency.
// Each stat is backed by an AtomicU64 and in zcache command each stat value type
// has a unique way to display itself (via StatValueTrait). Since stat values are
// a structure they need to implement clone, add_assign, and SubAssign. However,
// a display function is not required since that is handled by StatValueTrait.
//

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StatCount(pub AtomicU64);

impl StatCount {
    pub fn display_pretty(&self, scale: Option<f64>) {
        let mut value = self.0.load(Relaxed) as f64;
        if let Some(s) = scale {
            value *= s;
        }
        let nice_value = if value == 0.0 || (scale.is_none() && value < 999.0) {
            self.0.load(Relaxed).to_string()
        } else {
            nice_number_count(value)
        };

        // right aligned for a width of 6, padded with 2 spaces
        write_stdout!("{:>6}  ", nice_value);
    }
}

impl Clone for StatCount {
    fn clone(&self) -> Self {
        StatCount(AtomicU64::new(self.0.load(Ordering::Relaxed)))
    }
}

impl AddAssign<&Self> for StatCount {
    fn add_assign(&mut self, other: &Self) {
        self.0.fetch_add(other.0.load(Relaxed), Relaxed);
    }
}

impl AddAssign<u64> for StatCount {
    fn add_assign(&mut self, other: u64) {
        self.0.fetch_add(other, Relaxed);
    }
}

impl Sub<Self> for &StatCount {
    type Output = StatCount;

    fn sub(self, other: Self) -> Self::Output {
        StatCount(AtomicU64::new(self.0.load(Relaxed) - other.0.load(Relaxed)))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StatBytes(pub AtomicU64);

impl StatBytes {
    pub fn display_pretty(&self, scale: Option<f64>) {
        let mut value = self.0.load(Relaxed) as f64;
        if let Some(s) = scale {
            value *= s;
        }
        let nice_value = if value < 1.0 {
            // Intentionally avoid displaying "0B" when 0
            String::from("0")
        } else {
            nice_p2size(value.round().to_u64().unwrap())
        };
        // right aligned for a width of 6, padded with 2 spaces
        write_stdout!("{:>6}  ", nice_value);
    }
}

impl Clone for StatBytes {
    fn clone(&self) -> Self {
        StatBytes(AtomicU64::new(self.0.load(Ordering::Relaxed)))
    }
}

impl AddAssign<&Self> for StatBytes {
    fn add_assign(&mut self, other: &Self) {
        self.0.fetch_add(other.0.load(Relaxed), Relaxed);
    }
}

impl Sub<Self> for &StatBytes {
    type Output = StatBytes;

    fn sub(self, other: Self) -> Self::Output {
        StatBytes(AtomicU64::new(self.0.load(Relaxed) - other.0.load(Relaxed)))
    }
}

#[derive(Debug)]
pub struct StatLatency(pub Duration);

impl StatLatency {
    pub fn display_pretty(&self) {
        // right aligned for a width of 6, padded with 2 spaces
        write_stdout!("{:>6}  ", nice_number_time(self.0));
    }
}

//
// There are two types of histogram stats: LatencyHistogram and RequestHistogram
//

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LatencyHistogram(pub [StatCount; LatencyHistogram::BUCKETS]);

impl LatencyHistogram {
    pub const BUCKETS: usize = 28;
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        LatencyHistogram(arr![StatCount::default(); 28])
    }
}

impl Sub<Self> for &LatencyHistogram {
    type Output = LatencyHistogram;

    fn sub(self, other: Self) -> Self::Output {
        let mut difference = LatencyHistogram::default();
        for i in 0..self.0.len() {
            difference.0[i] = &self.0[i] - &other.0[i];
        }
        difference
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestHistogram(pub [StatCount; RequestHistogram::BUCKETS]);
impl RequestHistogram {
    pub const BUCKETS: usize = 16;
}
impl Default for RequestHistogram {
    fn default() -> Self {
        RequestHistogram(arr![StatCount::default(); 16])
    }
}
impl Sub<Self> for &RequestHistogram {
    type Output = RequestHistogram;

    fn sub(self, other: Self) -> Self::Output {
        let mut difference = RequestHistogram::default();
        for i in 0..self.0.len() {
            difference.0[i] = &self.0[i] - &other.0[i];
        }
        difference
    }
}

/// Collected I/O stat values, one for each DiskIoType.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IoStatValues {
    pub operations: StatCount,
    pub total_bytes: StatBytes,
    pub active_count: StatCount,
    pub total_nanoseconds: StatCount,
    pub latency_histogram: LatencyHistogram,
    pub request_histogram: RequestHistogram,
}

impl AddAssign<&Self> for IoStatValues {
    fn add_assign(&mut self, other: &Self) {
        self.operations += &other.operations;
        self.total_bytes += &other.total_bytes;
        self.active_count += &other.active_count;
        self.total_nanoseconds += &other.total_nanoseconds;

        for (s, o) in self
            .latency_histogram
            .0
            .iter_mut()
            .zip(other.latency_histogram.0.iter())
        {
            *s += o;
        }
        for (s, o) in self
            .request_histogram
            .0
            .iter_mut()
            .zip(other.request_histogram.0.iter())
        {
            *s += o;
        }
    }
}

impl Sub<Self> for &IoStatValues {
    type Output = IoStatValues;

    fn sub(self, other: Self) -> Self::Output {
        IoStatValues {
            operations: &self.operations - &other.operations,
            total_bytes: &self.total_bytes - &other.total_bytes,
            active_count: self.active_count.clone(),
            total_nanoseconds: &self.total_nanoseconds - &other.total_nanoseconds,
            latency_histogram: &self.latency_histogram - &other.latency_histogram,
            request_histogram: &self.request_histogram - &other.request_histogram,
        }
    }
}

/// The collection of IoStatValues for each disk
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskIoStats {
    pub name: String,
    pub stats: EnumMap<DiskIoType, IoStatValues>,
}

impl DiskIoStats {
    pub fn new(name: String) -> DiskIoStats {
        DiskIoStats {
            name,
            stats: Default::default(),
        }
    }
}

impl AddAssign<&Self> for DiskIoStats {
    fn add_assign(&mut self, other: &Self) {
        for (s, o) in self.stats.iter_mut().zip(other.stats.iter()) {
            *s.1 += o.1;
        }
    }
}

impl Sub<Self> for &DiskIoStats {
    type Output = DiskIoStats;

    fn sub(self, other: Self) -> Self::Output {
        let mut difference = DiskIoStats::new(self.name.clone());

        for (((_, self_values), (_, other_values)), (_, diff_values)) in self
            .stats
            .iter()
            .zip(other.stats.iter())
            .zip(difference.stats.iter_mut())
        {
            *diff_values = self_values - other_values;
        }

        difference
    }
}

/// A snapshot of the I/O stats collected from the zettacache.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IoStats {
    pub cache_runtime_id: Uuid, // must match before timestamps & disk_stats can be compared
    pub timestamp: Duration,
    pub disk_stats: Vec<DiskIoStats>,
}

#[derive(Debug, Serialize)]
pub struct IoStatsRef<'a> {
    pub cache_runtime_id: Uuid,
    pub timestamp: Duration,
    pub disk_stats: Vec<&'a DiskIoStats>,
}

impl Sub<Self> for &IoStats {
    type Output = IoStats;

    /// Subtract two IoStats. Used to create the net values between two IoStats samples.
    fn sub(self, other: Self) -> IoStats {
        if other.disk_stats.is_empty() {
            return self.clone();
        }
        assert_eq!(self.cache_runtime_id, other.cache_runtime_id);

        let mut difference = IoStats {
            timestamp: self.timestamp - other.timestamp,
            ..Default::default()
        };

        assert_eq!(self.disk_stats.len(), other.disk_stats.len());
        for (self_stat, other_stat) in self.disk_stats.iter().zip(other.disk_stats.iter()) {
            assert_eq!(self_stat.name, other_stat.name);
            difference.disk_stats.push(self_stat - other_stat);
        }

        difference
    }
}

impl IoStats {
    pub fn max_name_len(&self) -> usize {
        self.disk_stats
            .iter()
            .max_by_key(|stats| stats.name.len())
            .unwrap()
            .name
            .len()
    }
}

//
// Stat structs for zcache stats subcommand
//

#[derive(Debug, Enum, Clone, Serialize, Deserialize)]
/// The stats collected for zcache stats subcommand.
pub enum CacheStatCounter {
    // These stats are collected as part of the ZettaCache.stats struct.
    // They are consumed in the StatsDisplay.display_stat_values() function.
    Lookup,
    CacheMissLockBusy, // pending, DOSE-905
    IndexHitPendingChanges,
    IndexHitIndexCache,
    IndexHitChunkCache,
    IndexHitDisk,
    CacheHit,
    CacheHitBytes,
    InsertBytes,
    InsertForRead,
    InsertForWrite,
    InsertForSpeculativeRead,
    InsertForHeal,
    InsertDropBufferFull,
    InsertDropCacheFull,
    HealedBlocks,
    PendingChanges,
    Evictions,
    DemandBufferBytesAvailable,
    SpeculativeBufferBytesAvailable,
    SlabCapacity,
    AvailableSpace,
    AvailableBlocksSize,
    AvailableSlabsSize,
}

impl Display for CacheStatCounter {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// A snapshot of the cache stats collected from the zettacache.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub cache_runtime_id: Uuid, // must match before timestamp & stats can be compared
    pub timestamp: Duration,
    pub stats: EnumMap<CacheStatCounter, StatCount>,
}

impl CacheStats {
    pub fn new() -> CacheStats {
        CacheStats {
            cache_runtime_id: Default::default(),
            timestamp: Duration::default(),
            stats: Default::default(),
        }
    }
    pub fn value(&self, counter: CacheStatCounter) -> u64 {
        self.stats[counter].0.load(Relaxed)
    }

    pub fn track_count(&self, stat: CacheStatCounter) {
        self.stats[stat].0.fetch_add(1, Relaxed);
    }

    pub fn track_bytes(&self, stat: CacheStatCounter, bytes: u64) {
        self.stats[stat].0.fetch_add(bytes, Relaxed);
    }

    pub fn track_instantaneous(&self, stat: CacheStatCounter, value: u64) {
        self.stats[stat].0.store(value, Relaxed);
    }
}

impl Sub<&Self> for &CacheStats {
    type Output = CacheStats;

    /// Subtract two CacheStats. Used to create the net values between two samples.
    fn sub(self, other: &Self) -> CacheStats {
        assert_eq!(self.cache_runtime_id, other.cache_runtime_id);

        let mut difference = CacheStats {
            timestamp: self.timestamp - other.timestamp,
            ..Default::default()
        };

        for (((counter_type, self_stat), other_stat), diff_stat) in self
            .stats
            .iter()
            .zip(other.stats.values())
            .zip(difference.stats.values_mut())
        {
            match counter_type {
                // The following are instantaneous values and don't require subtraction
                CacheStatCounter::PendingChanges
                | CacheStatCounter::DemandBufferBytesAvailable
                | CacheStatCounter::SpeculativeBufferBytesAvailable
                | CacheStatCounter::SlabCapacity
                | CacheStatCounter::AvailableSpace
                | CacheStatCounter::AvailableSlabsSize
                | CacheStatCounter::AvailableBlocksSize => {
                    *diff_stat = self_stat.clone();
                }
                // Everything else should be subtracted
                _ => {
                    *diff_stat = self_stat - other_stat;
                }
            }
        }
        difference
    }
}
