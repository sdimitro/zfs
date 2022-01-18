use crate::base_types::Atime;
use crate::index::IndexValue;
use log::*;
use more_asserts::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
/// This data structure records the number of bytes cached
/// quantized by the atime they were inserted or last referenced.
/// History of evicted cache content is retained as "ghost" data:
/// buckets prior to "first_live" represent this data. "first ghost"
/// is the oldest (first) bucket in the histogram.
pub struct AtimeHistogramPhys {
    histogram: Vec<u64>,
    first_ghost: Atime,
    first_live: Atime,
}

impl AtimeHistogramPhys {
    pub fn new(first_ghost: Atime, first_live: Atime) -> AtimeHistogramPhys {
        AtimeHistogramPhys {
            histogram: Default::default(),
            first_ghost,
            first_live,
        }
    }

    pub fn first(&self) -> Atime {
        self.first_ghost
    }

    pub fn first_live(&self) -> Atime {
        self.first_live
    }

    /// Reset the start to a later atime, discarding older entries.
    /// Requests to reset to an earlier atime are ignored.
    pub fn reset_first(&mut self, new_first: Atime) {
        if new_first <= self.first_ghost {
            return;
        }
        let delta = new_first - self.first_ghost;
        // XXX - if this becomes a bottleneck we should change the histogram to a VecDeque
        // so that we don't have to copy when deleting the head of the histogram
        self.histogram.drain(0..delta);
        self.first_ghost = new_first;
    }

    /// Reset the live start to a later atime.
    /// Requests to reset to an earlier atime are ignored.
    pub fn reset_first_live(&mut self, new_first: Atime) {
        if new_first <= self.first_live {
            return;
        }
        self.first_live = new_first;
    }

    pub fn atime_for_target_size(&self, target_size: u64) -> Atime {
        info!(
            "histogram starts at {:?} (live at {:?}) and has {} entries",
            self.first_ghost,
            self.first_live,
            self.histogram.len()
        );
        let mut remaining = target_size;
        for (index, &bytes) in self.histogram.iter().enumerate().rev() {
            if remaining <= bytes {
                trace!("found target size {} at bucket {}", target_size, index);
                return self.first_ghost + index;
            }
            remaining -= bytes;
        }
        trace!(
            "cache smaller than target size {} by {} bytes",
            target_size,
            remaining
        );
        self.first_ghost
    }

    pub fn insert(&mut self, value: IndexValue) {
        assert_ge!(value.atime, self.first_ghost);
        let index = value.atime - self.first_ghost;
        if index >= self.histogram.len() {
            self.histogram.resize(index + 1, 0);
        }
        self.histogram[index] += u64::from(value.size);
    }

    pub fn remove(&mut self, value: IndexValue) {
        assert_ge!(value.atime, self.first_ghost);
        let index = value.atime - self.first_ghost;
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
        let index = atime - self.first_ghost;
        self.histogram[index..].iter().sum()
    }
}
