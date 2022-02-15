use crate::base_types::Atime;
use crate::index::IndexValue;
use derivative::Derivative;
use lazy_static::lazy_static;
use log::*;
use more_asserts::*;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::iter;
use std::mem;
use std::ops::AddAssign;
use std::ops::SubAssign;
use util::get_tunable;
use util::nice_p2size;
use util::BinaryIndexTree;

lazy_static! {
    // How many extra nodes to add to the binary index tree each time we grow it.
    pub static ref ATIME_BIT_EXPANSION_NODES: usize = get_tunable("atime_bit_expansion_nodes", 10);
}

#[derive(Serialize, Deserialize, Clone, Derivative)]
#[derivative(Debug)]
/// This data structure records the number of bytes cached
/// quantized by the atime they were inserted or last referenced.
/// History of evicted cache content is retained as "ghost" data:
/// buckets prior to "first_live" represent this data. "first ghost"
/// is the oldest (first) bucket in the histogram.
pub struct AtimeHistogramPhys {
    #[derivative(Debug(format_with = "util::tersevec"))]
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

    pub fn first_ghost(&self) -> Atime {
        self.first_ghost
    }

    pub fn first_live(&self) -> Atime {
        self.first_live
    }

    /// Replace self with an empty version, returning the previous value.  The
    /// first_ghost/live are preserved.
    pub fn take(&mut self) -> Self {
        mem::replace(self, Self::new(self.first_ghost, self.first_live))
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

    pub fn is_empty(&self) -> bool {
        let total_size: u64 = self.histogram[0..].iter().sum();
        total_size == 0
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

impl SubAssign<&Self> for AtimeHistogramPhys {
    fn sub_assign(&mut self, rhs: &Self) {
        // rhs must cover a subset of our Atimes
        assert_le!(self.first_ghost, rhs.first_ghost);
        assert_ge!(
            self.first_ghost + self.histogram.len(),
            rhs.first_ghost + self.histogram.len()
        );
        for (self_value, rhs_value) in self.histogram[rhs.first_ghost - self.first_ghost..]
            .iter_mut()
            .zip(rhs.histogram.iter())
        {
            *self_value -= *rhs_value;
        }
    }
}

/// Add the contents of two histograms, possibly extending the start or end.
impl AddAssign<&Self> for AtimeHistogramPhys {
    fn add_assign(&mut self, rhs: &Self) {
        if let Some(prepend) = self.first_ghost.checked_sub(rhs.first_ghost) {
            let mut new_histogram: Vec<u64> = vec![0; prepend];
            new_histogram.extend_from_slice(&self.histogram);
            self.histogram = new_histogram;
            self.first_ghost = rhs.first_ghost;
        }
        if let Some(append) = (rhs.first_ghost + rhs.histogram.len())
            .checked_sub(self.first_ghost + self.histogram.len())
        {
            self.histogram.extend(iter::repeat(0).take(append));
        }
        // rhs must now cover a subset of our Atimes
        assert_le!(self.first_ghost, rhs.first_ghost);
        assert_ge!(
            self.first_ghost + self.histogram.len(),
            rhs.first_ghost + rhs.histogram.len()
        );
        for (self_value, rhs_value) in self.histogram[rhs.first_ghost - self.first_ghost..]
            .iter_mut()
            .zip(rhs.histogram.iter())
        {
            *self_value += *rhs_value;
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

/// `AtimeHistogram` records the number of bytes cached quantized by the atime
/// they were inserted or last referenced. History of evicted cache content is
/// retained as "ghost" data: buckets prior to "first_live" represent this data.
/// "first ghost" is the oldest (first) bucket in the histogram.
pub struct AtimeHistogram {
    phys: AtimeHistogramPhys,
    tree: BinaryIndexTree,
}

impl AtimeHistogram {
    pub fn new(phys: AtimeHistogramPhys) -> AtimeHistogram {
        AtimeHistogram {
            tree: BinaryIndexTree::new(&phys.histogram, Some(*ATIME_BIT_EXPANSION_NODES)),
            phys,
        }
    }

    #[allow(dead_code)]
    pub fn to_phys(&self) -> AtimeHistogramPhys {
        self.phys.clone()
    }

    pub fn first_ghost(&self) -> Atime {
        self.phys.first_ghost
    }

    pub fn first_live(&self) -> Atime {
        self.phys.first_live
    }

    /// Reset the start to a later atime, discarding older entries.
    /// Requests to reset to an earlier atime are ignored.
    pub fn reset_first(&mut self, new_first: Atime) {
        if new_first <= self.phys.first_ghost {
            return;
        }
        debug!(
            "reset_first: new_first {:?} first_ghost {:?} in {:?}, {} histogram entries",
            new_first,
            self.phys.first_ghost,
            self,
            self.phys.histogram.len()
        );
        let delta = new_first - self.phys.first_ghost;
        // XXX - if this becomes a bottleneck we should change the histogram to a VecDeque
        // so that we don't have to copy when deleting the head of the histogram
        self.phys.histogram.drain(0..delta);
        self.phys.first_ghost = new_first;
        self.tree = BinaryIndexTree::new(&self.phys.histogram, Some(*ATIME_BIT_EXPANSION_NODES));
    }

    /// Reset the live start to a later atime.
    /// Requests to reset to an earlier atime are ignored.
    pub fn reset_first_live(&mut self, new_first: Atime) {
        if new_first <= self.phys.first_live {
            return;
        }
        self.phys.first_live = new_first;
    }

    pub fn atime_for_eviction_target(&self, eviction_size: u64) -> Atime {
        let result = self
            .phys
            .atime_for_target(self.phys.first_live, eviction_size);
        debug!(
            "histogram live start at {:?} with {} entries, evicting {} has {:?}",
            self.phys.first_live,
            self.phys.histogram.len(),
            nice_p2size(eviction_size),
            result,
        );
        result
    }

    pub fn atime_for_ghost_target(&self, ghost_reduction: u64) -> Atime {
        let atime = self
            .phys
            .atime_for_target(self.phys.first_ghost, ghost_reduction);
        debug!(
            "ghost size reduction of {} moves start from {:?} to {:?}",
            nice_p2size(ghost_reduction),
            self.phys.first_ghost,
            atime
        );
        std::cmp::min(atime, self.phys.first_live)
    }

    pub fn insert(&mut self, value: IndexValue) {
        self.phys.insert(value);
        let index = value.atime() - self.phys.first_ghost;
        if index >= self.tree.len() {
            // Note -- BinaryIndexTree::new() is O(n log n), this is mitigated
            // by growing the index in increments > 1
            self.tree =
                BinaryIndexTree::new(&self.phys.histogram, Some(*ATIME_BIT_EXPANSION_NODES));
            debug!("AtimeHistogram expanding BIT {:?}", self);
        } else {
            self.tree.insert_at(index, u64::from(value.size()));
        }
    }

    pub fn remove(&mut self, value: IndexValue) {
        self.phys.remove(value);
        let index = value.atime() - self.phys.first_ghost;
        self.tree.remove_at(index, u64::from(value.size()));
    }

    pub fn sum_live(&self) -> u64 {
        self.size_at(self.phys.first_live)
    }

    pub fn sum_ghost(&self) -> u64 {
        // Note we want a prefix sum here.  This is_O_(2 log n)
        self.size_at(self.phys.first_ghost) - self.size_at(self.phys.first_live)
    }

    /// Add up all the atime histogram buckets from key atime up to the current
    /// atime. This will be the minimum cache size that would contain this key.
    pub fn size_at(&self, atime: Atime) -> u64 {
        // Note this interface is called for every cache hit. A BinaryIndexTree
        // (aka Fenwick tree) is used to obtain the sum in O(log(N)).
        let index = atime - self.phys.first_ghost;
        self.tree.suffix_sum(index)
    }
}

impl std::fmt::Debug for AtimeHistogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtimeHistogram")
            .field("histogram_buckets", &self.phys.histogram.len())
            .field("binary_insert_tree_len", &self.tree.len())
            .field("first_ghost", &self.phys.first_ghost.0)
            .field("first_live", &self.phys.first_live.0)
            .finish()
    }
}
