use std::collections::VecDeque;
use std::fmt::Debug;
use std::ops::Add;
use std::ops::Sub;

use more_asserts::*;

/// This is a data structure that allows for unordered insertions, and ordered removals of
/// *contiguous* entries.  Contiguity is determined by `key == key + 1`.  We get very good
/// performance by using a VecDeque rather than a BTreeMap, because eventually all contiguous
/// entries are inserted,
pub struct Unordered<K, V> {
    first: K,
    pending: VecDeque<Option<V>>,
}

impl<K, V> Unordered<K, V>
where
    K: Copy + Debug + PartialOrd + Sub<K, Output = usize> + Add<usize, Output = K>,
{
    /// `first` is the first key that can be returned from pop().
    pub fn new(first: K) -> Self {
        Self {
            first,
            pending: Default::default(),
        }
    }

    /// Panics if the key is before the first value, or already present.
    pub fn insert(&mut self, key: K, value: V) {
        assert_ge!(key, self.first);
        let index = key - self.first;
        if index >= self.pending.len() {
            self.pending.resize_with(index + 1, || None);
        }
        match &mut self.pending[index] {
            Some(_) => panic!("insert({:?}) found entry already present", key),
            v @ None => *v = Some(value),
        }
    }

    /// Returns None if the next contiguous value has not been inserted yet.
    pub fn pop(&mut self) -> Option<(K, V)> {
        if matches!(self.pending.front(), Some(Some(_))) {
            let first = self.first;
            self.first = self.first + 1;
            Some((first, self.pending.pop_front().unwrap().unwrap()))
        } else {
            None
        }
    }

    /// Returns None if the next contiguous value has not been inserted yet.
    pub fn peek(&self) -> Option<K> {
        if matches!(self.pending.front(), Some(Some(_))) {
            Some(self.first)
        } else {
            None
        }
    }

    /// Remove and return all the entries less than `limit`.  The entries do not
    /// need to be contiguous; non-present entries will be skipped.
    pub fn drain(&mut self, limit: K) -> impl Iterator<Item = (K, V)> + '_ {
        assert_ge!(limit, self.first);
        let index = limit - self.first;
        if index >= self.pending.len() {
            self.pending.resize_with(index, || None);
        }
        self.pending.drain(..index).filter_map(|opt| {
            let result = opt.map(|value| (self.first, value));
            self.first = self.first + 1;
            result
        })
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn last(&self) -> K {
        self.first + self.pending.len()
    }
}
