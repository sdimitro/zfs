use crate::base_types::Atime;
use crate::index::IndexValue;
use log::*;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use util::nice_p2size;

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

    /// Given an atime "starting point", calculate the "end" atime such that
    /// self.histogram[start..end].sum() is >= the provided target value or
    /// return the next atime past the end of the vector if sum() < "target"
    /// data in the vector. Return "start" if the target value is 0.
    fn atime_for_target(&self, start: Atime, target: u64) -> Atime {
        if target == 0 {
            return start;
        }
        let mut end = start;
        let mut remaining = target;
        for &bytes in self.histogram[start - self.first_ghost..].iter() {
            end = end.next();
            if remaining <= bytes {
                break;
            }
            remaining -= bytes;
        }
        end
    }

    pub fn atime_for_eviction_target(&self, eviction_size: u64) -> Atime {
        debug!(
            "histogram live start at {:?} with {} entries, evicting {}MB",
            self.first_live,
            self.histogram.len(),
            eviction_size / 1024 / 1024,
        );
        self.atime_for_target(self.first_live, eviction_size)
    }

    pub fn atime_for_ghost_target(&self, ghost_reduction: u64) -> Atime {
        let atime = self.atime_for_target(self.first_ghost, ghost_reduction);
        debug!(
            "ghost size reduction of {}MB moves start from {:?} to {:?}",
            ghost_reduction / 1024 / 1024,
            self.first_ghost,
            atime
        );
        std::cmp::min(atime, self.first_live)
    }

    pub fn insert(&mut self, value: IndexValue) {
        let index = value.atime() - self.first_ghost;
        if index >= self.histogram.len() {
            self.histogram.resize(index + 1, 0);
        }
        self.histogram[index] += u64::from(value.size());
    }

    pub fn remove(&mut self, value: IndexValue) {
        let index = value.atime() - self.first_ghost;
        self.histogram[index] -= u64::from(value.size());
    }

    pub fn clear(&mut self) {
        self.histogram.clear();
    }

    pub fn sum_live(&self) -> u64 {
        self.size_at(self.first_live)
    }

    pub fn sum_ghost(&self) -> u64 {
        let index = self.first_live - self.first_ghost;
        self.histogram[..index].iter().sum()
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

    pub fn assert_eq(&self, other: &AtimeHistogramPhys) {
        assert_eq!(self.first_ghost, other.first_ghost);
        assert_eq!(self.first_live, other.first_live);
        assert_eq!(self.histogram.len(), other.histogram.len());
        for (index, (&value, &other_value)) in self
            .histogram
            .iter()
            .zip(other.histogram.iter())
            .enumerate()
        {
            assert_eq!(
                value,
                other_value,
                "index {} ({:?}) does not match",
                index,
                self.first_ghost + index
            );
        }
    }
}

impl Display for AtimeHistogramPhys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "AtimeHistogramPhys(first_ghost={:?}, first_live={:?}):",
            self.first_ghost, self.first_live
        )?;
        for (index, &value) in self.histogram.iter().enumerate() {
            writeln!(
                f,
                "    [{:?}] = {} ({})",
                self.first_ghost + index,
                nice_p2size(value),
                value
            )?;
        }
        Ok(())
    }
}
