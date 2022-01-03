use crate::base_types::Atime;
use crate::index::IndexValue;
use log::*;
use more_asserts::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct AtimeHistogramPhys {
    histogram: Vec<u64>,
    first: Atime,
}

impl AtimeHistogramPhys {
    pub fn new(first: Atime) -> AtimeHistogramPhys {
        AtimeHistogramPhys {
            histogram: Default::default(),
            first,
        }
    }

    pub fn first(&self) -> Atime {
        self.first
    }

    /// Reset the start to a later atime, discarding older entries.
    /// Requests to reset to an earlier atime are ignored.
    pub fn reset_first(&mut self, new_first: Atime) {
        if new_first <= self.first {
            return;
        }
        let delta = new_first - self.first;
        // XXX - if this becomes a bottleneck we should change the histogram to a VecDeque
        // so that we don't have to copy when deleting the head of the histogram
        self.histogram.drain(0..delta);
        self.first = new_first;
    }

    pub fn atime_for_target_size(&self, target_size: u64) -> Atime {
        info!(
            "histogram starts at {:?} and has {} entries",
            self.first,
            self.histogram.len()
        );
        let mut remaining = target_size;
        for (index, &bytes) in self.histogram.iter().enumerate().rev() {
            if remaining <= bytes {
                trace!("final include of {} for target at bucket {}", bytes, index);
                return self.first + index;
            }
            trace!("including {} in target at bucket {}", bytes, index);
            remaining -= bytes;
        }
        self.first
    }

    pub fn insert(&mut self, value: IndexValue) {
        assert_ge!(value.atime, self.first);
        let index = value.atime - self.first;
        if index >= self.histogram.len() {
            self.histogram.resize(index + 1, 0);
        }
        self.histogram[index] += u64::from(value.size);
    }

    pub fn remove(&mut self, value: IndexValue) {
        assert_ge!(value.atime, self.first);
        let index = value.atime - self.first;
        self.histogram[index] -= u64::from(value.size);
    }

    pub fn clear(&mut self) {
        self.histogram.clear();
    }

    pub fn sum(&self) -> u64 {
        self.histogram.iter().sum()
    }

    /// Add up all the atime histogram buckets from key atime
    /// to the current atime. This will be the minimum cache size
    /// that would contain this key.
    pub fn size_at(&self, atime: Atime) -> u64 {
        // XXX - Note this interface is called for every cache hit. It is, currently, an O(N) algorithm
        // (adding all of the elements of the array). This may be an issue for very large atime histograms.
        // This could be improved to O(log(N)) with something like a segment tree.
        let index = atime - self.first;
        self.histogram[index..].iter().sum()
    }
}
