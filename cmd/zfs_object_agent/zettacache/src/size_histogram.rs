use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SizeHistogramPhys {
    start: SystemTime,
    pub lookups: u64,
    pub cache_capacity: u64,
    pub meta_overhead: u64,
    pub bucket_size: u64,
    pub histogram: Vec<u64>,
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
            bucket_size: histogram_range / quantiles as u64,
            histogram: vec![0; quantiles],
        }
    }

    /// Record a "hit" in the appropriate size bucket (taking into account the cache metadata overhead)
    pub fn hit(&mut self, size_at_hit: u64) {
        let index = usize::try_from((size_at_hit + self.meta_overhead) / self.bucket_size).unwrap();
        // The histogram may not be large enough if we've expanded the capacity
        // since the histogram was created (e.g. by adding disks).
        if let Some(value) = self.histogram.get_mut(index) {
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
