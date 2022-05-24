use std::time::SystemTime;

use derivative::Derivative;
use serde::Deserialize;
use serde::Serialize;
use util::From64;

#[derive(Serialize, Deserialize, Clone, Derivative)]
#[derivative(Debug)]
pub struct SizeHistogramPhys {
    start: SystemTime,
    pub lookups: u64,
    pub cache_capacity: u64,
    pub meta_overhead: u64,
    pub bucket_size: u64,
    #[derivative(Debug(format_with = "util::tersevec"))]
    pub live_histogram: Vec<u64>,
    #[derivative(Debug(format_with = "util::tersevec"))]
    pub ghost_histogram: Vec<u64>,
}

impl SizeHistogramPhys {
    /// The histogram range is the sum of the physical cache capacity (cache_capacity)
    /// plus the additional capacity being tracked for cache ghost hits.
    pub fn new(
        histogram_range: u64,
        cache_capacity: u64,
        meta_overhead: u64,
        quantiles: usize,
    ) -> Self {
        Self {
            start: SystemTime::now(),
            lookups: 0,
            cache_capacity,
            meta_overhead,
            bucket_size: if quantiles == 0 {
                0
            } else {
                histogram_range / quantiles as u64
            },
            live_histogram: vec![0; quantiles],
            ghost_histogram: vec![0; quantiles],
        }
    }

    /// Record a "hit" in the appropriate size bucket (taking into account the cache metadata
    /// overhead)
    pub fn live_hit(&mut self, size_at_hit: u64) {
        let index = usize::from64((size_at_hit + self.meta_overhead) / self.bucket_size);
        // The histogram may not be large enough if we've expanded the capacity
        // since the histogram was created (e.g. by adding disks).
        if let Some(value) = self.live_histogram.get_mut(index) {
            *value += 1;
        }
    }

    /// Record a ghost "hit" in the appropriate size bucket
    pub fn ghost_hit(&mut self, size_at_hit: u64) {
        let index = usize::from64((size_at_hit + self.meta_overhead) / self.bucket_size);
        // The histogram may not be large enough if we've expanded the capacity
        // since the histogram was created (e.g. by adding disks).
        if let Some(value) = self.ghost_histogram.get_mut(index) {
            *value += 1;
        }
    }

    pub fn lookup(&mut self) {
        self.lookups += 1;
    }

    pub fn started(&self) -> SystemTime {
        self.start
    }
}
