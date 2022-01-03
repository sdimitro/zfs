use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SizeHistogramPhys {
    start: SystemTime,
    pub lookups: u64,
    pub bucket_size: u64,
    pub histogram: Vec<u64>,
}

impl SizeHistogramPhys {
    pub fn new(cache_max_size: u64, quantiles: usize) -> Self {
        Self {
            start: SystemTime::now(),
            lookups: 0,
            bucket_size: cache_max_size / quantiles as u64,
            histogram: vec![0; quantiles],
        }
    }

    pub fn hit(&mut self, size_at_hit: u64) {
        let index = usize::try_from(size_at_hit / self.bucket_size).unwrap();
        self.histogram[index] += 1;
    }

    pub fn lookup(&mut self) {
        self.lookups += 1;
    }

    pub fn started(&self) -> SystemTime {
        self.start
    }
}
