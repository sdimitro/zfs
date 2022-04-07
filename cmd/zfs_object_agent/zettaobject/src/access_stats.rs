use std::collections::HashMap;
use std::fmt::Display;
use std::fmt::Formatter;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use arr_macro::arr;
use enum_map::Enum;
use enum_map::EnumMap;
use tokio::sync::AcquireError;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use util::tunable;

tunable! {
    pub static ref OBJECT_QUEUE_DEPTH_PER_TYPE: usize = 100;
}

#[derive(Debug, Enum, Copy, Clone)]
pub enum ObjectAccessOpType {
    ReadsGet,
    TxgSyncPut,
    ReclaimGet,
    ReclaimPut,
    MetadataGet,
    MetadataPut,
    ObjectDelete,
}

#[derive(Debug, Enum)]
enum LatencyHistogramType {
    Gets,
    Puts,
    Deletes,
}

#[derive(Debug, Enum)]
enum RequestSizeHistogramType {
    Gets,
    Puts,
    Deletes,
}

impl Display for ObjectAccessOpType {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Display for LatencyHistogramType {
    // Note: display here is also used as our nvlist key
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "LatencyHistogram{:?}", self)
    }
}

impl Display for RequestSizeHistogramType {
    // Note: display here is also used as our nvlist key
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "RequestHistogram{:?}", self)
    }
}

// These are equivalent to VDEV_L_HISTO_BUCKETS and VDEV_RQ_HISTO_BUCKETS in zfs.h
pub const LATENCY_HISTOGRAM_BUCKETS: u32 = 37;
pub const REQUEST_SIZE_HISTOGRAM_BUCKETS: u32 = 25;

struct LatencyHistogram(pub [AtomicU64; LATENCY_HISTOGRAM_BUCKETS as usize]);
struct RequestSizeHistogram(pub [AtomicU64; REQUEST_SIZE_HISTOGRAM_BUCKETS as usize]);

impl Default for LatencyHistogram {
    fn default() -> Self {
        LatencyHistogram(arr![AtomicU64::default(); 37])
    }
}

impl Default for RequestSizeHistogram {
    fn default() -> Self {
        RequestSizeHistogram(arr![AtomicU64::default(); 25])
    }
}

#[derive(Default)]
struct StatTypeCounts {
    operations: AtomicU64,
    total_bytes: AtomicU64,
    active_count: AtomicU64,
}

pub struct ObjectAccessStats {
    timebase: Instant,
    counters: EnumMap<ObjectAccessOpType, StatTypeCounts>,
    latency_histograms: EnumMap<LatencyHistogramType, LatencyHistogram>,
    request_size_histograms: EnumMap<RequestSizeHistogramType, RequestSizeHistogram>,
}

#[must_use]
pub struct OpInProgress<'a> {
    stat_type: ObjectAccessOpType,
    begin: Instant,
    stats: &'a ObjectAccessStats,
}

impl<'a> OpInProgress<'a> {
    fn new(stat_type: ObjectAccessOpType, stats: &'a ObjectAccessStats) -> Self {
        stats.counters[stat_type]
            .active_count
            .fetch_add(1, Ordering::Relaxed);
        OpInProgress {
            stat_type,
            begin: Instant::now(),
            stats,
        }
    }

    fn end_impl(self, bytes: u64, operations: u64) {
        let latency = self.begin.elapsed().as_nanos();
        let counters = &self.stats.counters[self.stat_type];
        counters.operations.fetch_add(operations, Ordering::Relaxed);
        counters.total_bytes.fetch_add(bytes, Ordering::Relaxed);

        // This bucket mapping is equivalent to L_HISTO() macro in zfs.h
        let latency_bucket = std::cmp::min(
            latency.next_power_of_two().trailing_zeros(),
            LATENCY_HISTOGRAM_BUCKETS - 1,
        ) as usize;

        // This bucket mapping is equivalent to RQ_HISTO() macro in zfs.h
        let request_bucket = std::cmp::min(
            bytes.next_power_of_two().trailing_zeros(),
            REQUEST_SIZE_HISTOGRAM_BUCKETS - 1,
        ) as usize;

        // Map the ObjectAccessStatType to the corresponding histogram type
        let (latency_type, request_type) = match self.stat_type {
            ObjectAccessOpType::ReadsGet
            | ObjectAccessOpType::ReclaimGet
            | ObjectAccessOpType::MetadataGet => {
                (LatencyHistogramType::Gets, RequestSizeHistogramType::Gets)
            }
            ObjectAccessOpType::TxgSyncPut
            | ObjectAccessOpType::ReclaimPut
            | ObjectAccessOpType::MetadataPut => {
                (LatencyHistogramType::Puts, RequestSizeHistogramType::Puts)
            }
            ObjectAccessOpType::ObjectDelete => (
                LatencyHistogramType::Deletes,
                RequestSizeHistogramType::Deletes,
            ),
        };
        self.stats.latency_histograms[latency_type].0[latency_bucket]
            .fetch_add(operations, Ordering::Relaxed);
        self.stats.request_size_histograms[request_type].0[request_bucket]
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn end(self, bytes: u64) {
        self.end_impl(bytes, 1)
    }

    pub fn end_multiple(self, bytes: u64, operations: u64) {
        self.end_impl(bytes, operations)
    }
}

impl<'a> Drop for OpInProgress<'a> {
    fn drop(&mut self) {
        let counters = &self.stats.counters[self.stat_type];
        counters.active_count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for ObjectAccessStats {
    fn default() -> Self {
        ObjectAccessStats {
            timebase: Instant::now(),
            counters: Default::default(),
            latency_histograms: Default::default(),
            request_size_histograms: Default::default(),
        }
    }
}

impl ObjectAccessStats {
    pub fn begin(&self, stat_type: ObjectAccessOpType) -> OpInProgress<'_> {
        OpInProgress::new(stat_type, self)
    }

    fn sum_stats(&self, stat_types: &[ObjectAccessOpType]) -> HashMap<String, u64> {
        let mut total = HashMap::new();

        total.insert(
            "operations".into(),
            stat_types
                .iter()
                .map(|&stat_type| self.counters[stat_type].operations.load(Ordering::Relaxed))
                .sum(),
        );
        total.insert(
            "total_bytes".into(),
            stat_types
                .iter()
                .map(|&stat_type| self.counters[stat_type].total_bytes.load(Ordering::Relaxed))
                .sum(),
        );
        total.insert(
            "active".into(),
            stat_types
                .iter()
                .map(|&stat_type| {
                    self.counters[stat_type]
                        .active_count
                        .load(Ordering::Relaxed)
                })
                .sum(),
        );

        total
    }

    pub fn collect_stats(&self) -> HashMap<String, StatMapValue> {
        let mut outer = HashMap::new();
        let order = Ordering::Relaxed;

        // Note: try_from() will always succeed since 2^64 ns is 580 years, and it's
        // inconceivable that the object agent could be running for that long.
        let timestamp = u64::try_from(self.timebase.elapsed().as_nanos()).unwrap();
        outer.insert("Timestamp".into(), StatMapValue::Counter(timestamp));

        // Add the named counters for each stat type
        for (t, s) in self.counters.iter() {
            let mut inner = HashMap::new();
            inner.insert("operations".into(), s.operations.load(order));
            inner.insert("total_bytes".into(), s.total_bytes.load(order));
            inner.insert("active".into(), s.active_count.load(order));
            outer.insert(t.to_string(), StatMapValue::CounterMap(inner));
        }

        // Add the histograms
        for (t, h) in self.latency_histograms.iter() {
            outer.insert(
                t.to_string(),
                StatMapValue::Histogram(h.0.iter().map(|v: &AtomicU64| v.load(order)).collect()),
            );
        }
        for (t, h) in self.request_size_histograms.iter() {
            outer.insert(
                t.to_string(),
                StatMapValue::Histogram(h.0.iter().map(|v: &AtomicU64| v.load(order)).collect()),
            );
        }

        // Sum the Gets and Puts into a total for each counter
        outer.insert(
            "TotalGet".into(),
            StatMapValue::CounterMap(self.sum_stats(&[
                ObjectAccessOpType::ReadsGet,
                ObjectAccessOpType::MetadataGet,
                ObjectAccessOpType::ReclaimGet,
            ])),
        );
        outer.insert(
            "TotalPut".into(),
            StatMapValue::CounterMap(self.sum_stats(&[
                ObjectAccessOpType::TxgSyncPut,
                ObjectAccessOpType::MetadataPut,
                ObjectAccessOpType::ReclaimPut,
            ])),
        );

        outer
    }
}

#[derive(Debug)]
pub enum StatMapValue {
    Counter(u64),
    CounterMap(HashMap<String, u64>),
    Histogram(Vec<u64>),
}
pub struct OutstandingOps(Semaphore);
impl Default for OutstandingOps {
    fn default() -> Self {
        Self(Semaphore::new(*OBJECT_QUEUE_DEPTH_PER_TYPE))
    }
}

impl OutstandingOps {
    pub async fn acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        self.0.acquire().await
    }
}
