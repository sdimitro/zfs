use more_asserts::*;
use std::{
    cmp::Ordering,
    collections::{btree_map::Iter, BTreeMap},
    iter::Fuse,
    ops::RangeBounds,
};

#[derive(Default)]
pub struct RangeTree {
    tree: BTreeMap<u64, u64>, // start -> size
    space: u64,
}

impl RangeTree {
    pub fn new() -> RangeTree {
        RangeTree {
            tree: BTreeMap::new(),
            space: 0,
        }
    }

    // panics if already present
    pub fn add(&mut self, start: u64, size: u64) {
        if size == 0 {
            return;
        }

        let end = start + size;
        let before = self.tree.range(..end).next_back();
        let after = self.tree.range(start..).next();

        let merge_before = match before {
            Some((&before_start, &before_size)) => {
                assert_le!(before_start + before_size, start);
                before_start + before_size == start
            }
            None => false,
        };

        let merge_after = match after {
            Some((&after_start, &_after_size)) => {
                assert_ge!(after_start, end);
                after_start == end
            }
            None => false,
        };

        if merge_before && merge_after {
            let &before_start = before.unwrap().0;
            let (&after_start, &after_size) = after.unwrap();
            self.tree
                .entry(before_start)
                .and_modify(|before_size| *before_size += size + after_size);
            self.tree.remove(&after_start);
        } else if merge_before {
            let before_start = *before.unwrap().0;
            self.tree
                .entry(before_start)
                .and_modify(|before_size| *before_size += size);
        } else if merge_after {
            let after_start = *after.unwrap().0;
            let after_size = *after.unwrap().1;
            self.tree.remove(&after_start);
            self.tree.insert(start, size + after_size);
        } else {
            self.tree.insert(start, size);
        }
        self.space += size;
    }

    // panics if not present
    pub fn remove(&mut self, start: u64, size: u64) {
        assert_ne!(size, 0);

        let end = start + size;
        let (&existing_start, existing_size_ref) = self.tree.range_mut(..end).next_back().unwrap();
        let existing_end = existing_start + *existing_size_ref;
        assert_le!(existing_start, start);
        assert_ge!(existing_end, end);
        let left_over = existing_start != start;
        let right_over = existing_end != end;

        if left_over && right_over {
            *existing_size_ref = start - existing_start;
            self.tree.insert(end, existing_end - end);
        } else if left_over {
            *existing_size_ref = start - existing_start;
        } else if right_over {
            self.tree.remove(&start);
            self.tree.insert(end, existing_end - end);
        } else {
            self.tree.remove(&start);
        }
        self.space -= size;
    }

    pub fn overlap(&self, start: u64, size: u64) -> Option<(u64, u64)> {
        assert_ne!(size, 0);

        let end = start + size;
        if let Some((&existing_start, &existing_size)) = self.tree.range(..end).next_back() {
            let existing_end = existing_start + existing_size;
            if existing_start <= start && existing_end >= end {
                return Some((existing_start, existing_size));
            }
        }

        None
    }

    pub fn verify_absent(&self, start: u64, size: u64) {
        if let Some((existing_start, existing_size)) = self.overlap(start, size) {
            panic!(
                "range_tree segment [{}, {}) is not absent (overlaps with segment [{}, {}))",
                start,
                start + size,
                existing_start,
                existing_start + existing_size
            );
        }
    }

    /// Returns Iter<start, size>
    pub fn iter(&self) -> std::collections::btree_map::Iter<u64, u64> {
        self.tree.iter()
    }

    pub fn iter_inverse(&self, start: u64, end: u64) -> RangeTreeInverseIter {
        RangeTreeInverseIter::new(self, start, end)
    }

    pub fn range<R>(&self, range: R) -> std::collections::btree_map::Range<'_, u64, u64>
    where
        R: RangeBounds<u64>,
    {
        self.tree.range(range)
    }

    pub fn clear(&mut self) {
        self.tree.clear();
        self.space = 0;
    }

    pub fn space(&self) -> u64 {
        self.space
    }

    pub fn verify_space(&self) {
        assert_eq!(self.space, self.tree.values().sum::<u64>())
    }
}

pub struct RangeTreeInverseIter<'a> {
    rt_iter: Fuse<Iter<'a, u64, u64>>,
    iter_end: u64,
    cursor: u64,
}

impl<'a> RangeTreeInverseIter<'a> {
    fn new(rtree: &RangeTree, start: u64, end: u64) -> RangeTreeInverseIter {
        assert_ge!(end, start);
        RangeTreeInverseIter {
            rt_iter: rtree.iter().fuse(),
            iter_end: end,
            cursor: start,
        }
    }
}

impl<'a> Iterator for RangeTreeInverseIter<'a> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        loop {
            if self.cursor >= self.iter_end {
                return None;
            }
            let c = self.cursor;
            match self.rt_iter.next() {
                Some((&start, &size)) => {
                    let end = start + size;
                    match start.cmp(&c) {
                        Ordering::Greater => {
                            if start >= self.iter_end {
                                self.cursor = self.iter_end;
                                return Some((c, self.iter_end - c));
                            } else {
                                self.cursor = end;
                                return Some((c, start - c));
                            }
                        }
                        Ordering::Equal => {
                            if end >= self.iter_end {
                                self.cursor = self.iter_end;
                                return None;
                            } else {
                                self.cursor = end;
                                continue;
                            }
                        }
                        Ordering::Less => {
                            if end >= self.iter_end {
                                self.cursor = self.iter_end;
                                return None;
                            } else if end >= c {
                                self.cursor = end;
                                continue;
                            } else {
                                continue;
                            }
                        }
                    }
                }
                None => {
                    self.cursor = self.iter_end;
                    return Some((c, self.iter_end - c));
                }
            }
        }
    }
}

#[cfg(test)]
mod test_iter_inverse {
    use super::*;

    fn validate_iter_inverse_ranges(
        a: &RangeTree,
        start: u64,
        end: u64,
        expected_space: u64,
        expected_nsegments: u64,
    ) {
        let mut total_segments = 0;
        let mut total_space = 0;
        for (_, size) in a.iter_inverse(start, end) {
            total_segments += 1;
            total_space += size;
        }
        assert_eq!(expected_nsegments, total_segments);
        assert_eq!(expected_space, total_space);
    }

    #[test]
    fn test_empty() {
        let a = RangeTree::new();
        validate_iter_inverse_ranges(&a, 0, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 1, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 0, 0);
    }

    #[test]
    fn test_single_start_unit_segment() {
        let mut a = RangeTree::new();
        a.add(0, 1);
        validate_iter_inverse_ranges(&a, 0, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 1, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 3, 1, 1);
    }

    #[test]
    fn test_single_start_range() {
        let mut a = RangeTree::new();
        a.add(0, 5);
        validate_iter_inverse_ranges(&a, 0, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 5, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 4, 5, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 10, 5, 1);
        validate_iter_inverse_ranges(&a, 5, 10, 5, 1);
    }

    #[test]
    fn test_single_middle_range() {
        let mut a = RangeTree::new();
        a.add(5, 3);
        validate_iter_inverse_ranges(&a, 0, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 0, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 8, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 10, 7, 2);

        validate_iter_inverse_ranges(&a, 5, 7, 0, 0);
        validate_iter_inverse_ranges(&a, 5, 8, 0, 0);
        validate_iter_inverse_ranges(&a, 5, 10, 2, 1);

        validate_iter_inverse_ranges(&a, 6, 7, 0, 0);
        validate_iter_inverse_ranges(&a, 6, 8, 0, 0);
        validate_iter_inverse_ranges(&a, 6, 10, 2, 1);

        validate_iter_inverse_ranges(&a, 8, 8, 0, 0);
        validate_iter_inverse_ranges(&a, 8, 9, 1, 1);
        validate_iter_inverse_ranges(&a, 8, 10, 2, 1);

        validate_iter_inverse_ranges(&a, 9, 10, 1, 1);
    }

    #[test]
    fn test_two_ranges_start_end() {
        let mut a = RangeTree::new();
        a.add(0, 1);
        a.add(9, 1);
        validate_iter_inverse_ranges(&a, 0, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 9, 8, 1);
        validate_iter_inverse_ranges(&a, 0, 10, 8, 1);

        validate_iter_inverse_ranges(&a, 1, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 1, 9, 8, 1);
        validate_iter_inverse_ranges(&a, 1, 10, 8, 1);

        validate_iter_inverse_ranges(&a, 8, 8, 0, 0);
        validate_iter_inverse_ranges(&a, 8, 9, 1, 1);
        validate_iter_inverse_ranges(&a, 8, 10, 1, 1);

        validate_iter_inverse_ranges(&a, 9, 9, 0, 0);
        validate_iter_inverse_ranges(&a, 9, 10, 0, 0);
    }

    #[test]
    fn test_two_ranges_middle_end() {
        let mut a = RangeTree::new();
        a.add(5, 1);
        a.add(9, 1);
        validate_iter_inverse_ranges(&a, 0, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 7, 6, 2);
        validate_iter_inverse_ranges(&a, 0, 9, 8, 2);
        validate_iter_inverse_ranges(&a, 0, 10, 8, 2);
        validate_iter_inverse_ranges(&a, 0, 11, 9, 3);
    }
}
