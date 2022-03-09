use std::cmp::Ordering;
use std::collections::btree_map::Iter;
use std::collections::BTreeMap;
use std::iter::Fuse;
use std::ops::Range;

use more_asserts::*;

#[derive(Default)]
pub struct BitRange {
    tree: BTreeMap<u16, u16>, // start -> size
    set_bits: u16,
}

impl BitRange {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn insert(&mut self, slot: u16) {
        self.insert_impl(slot, 1)
    }

    pub fn remove(&mut self, slot: u16) {
        self.remove_impl(slot, 1)
    }

    pub fn insert_range(&mut self, range: Range<u16>) {
        self.insert_impl(range.start, range.end - range.start)
    }

    pub fn remove_range(&mut self, range: Range<u16>) {
        self.remove_impl(range.start, range.end - range.start)
    }

    pub fn min(&self) -> Option<u16> {
        self.tree.keys().next().copied()
    }

    pub fn len(&self) -> u16 {
        self.set_bits
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, slot: u16) -> bool {
        self.overlap(slot, 1).is_some()
    }

    // panics if already present
    fn insert_impl(&mut self, start: u16, size: u16) {
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
        self.set_bits += size;
    }

    // panics if not present
    fn remove_impl(&mut self, start: u16, size: u16) {
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
        self.set_bits -= size;
    }

    fn overlap(&self, start: u16, size: u16) -> Option<(u16, u16)> {
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

    /// Returns ranges of set slots as (first slot set, # of slots in range)
    pub fn iter_ranges(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.tree.iter().map(|(x, y)| (*x, *y))
    }

    /// Returns ranges of unset slots as (first slot set, # of slots in range)
    pub fn iter_inverse_ranges(&self, start: u16, end: u16) -> BitRangeInverseIter {
        BitRangeInverseIter::new(self, start, end)
    }

    pub fn clear(&mut self) {
        self.tree.clear();
        self.set_bits = 0;
    }
}

pub struct BitRangeInverseIter<'a> {
    rt_iter: Fuse<Iter<'a, u16, u16>>,
    iter_end: u16,
    cursor: u16,
}

impl<'a> BitRangeInverseIter<'a> {
    fn new(rtree: &BitRange, start: u16, end: u16) -> BitRangeInverseIter {
        assert_ge!(end, start);
        BitRangeInverseIter {
            rt_iter: rtree.tree.iter().fuse(),
            iter_end: end,
            cursor: start,
        }
    }
}

impl<'a> Iterator for BitRangeInverseIter<'a> {
    type Item = (u16, u16);

    fn next(&mut self) -> Option<(u16, u16)> {
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
        a: &BitRange,
        start: u16,
        end: u16,
        expected_space: u16,
        expected_nsegments: u16,
    ) {
        let mut total_segments = 0;
        let mut total_space = 0;
        for (_, size) in a.iter_inverse_ranges(start, end) {
            total_segments += 1;
            total_space += size;
        }
        assert_eq!(expected_nsegments, total_segments);
        assert_eq!(expected_space, total_space);
    }

    #[test]
    fn test_empty() {
        let a = BitRange::new();
        validate_iter_inverse_ranges(&a, 0, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 1, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 0, 0);
    }

    #[test]
    fn test_single_start_unit_segment() {
        let mut a = BitRange::new();
        a.insert_impl(0, 1);
        validate_iter_inverse_ranges(&a, 0, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 1, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 3, 1, 1);
    }

    #[test]
    fn test_single_start_range() {
        let mut a = BitRange::new();
        a.insert_impl(0, 5);
        validate_iter_inverse_ranges(&a, 0, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 5, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 4, 5, 0, 0);
        validate_iter_inverse_ranges(&a, 0, 10, 5, 1);
        validate_iter_inverse_ranges(&a, 5, 10, 5, 1);
    }

    #[test]
    fn test_single_middle_range() {
        let mut a = BitRange::new();
        a.insert_impl(5, 3);
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
        let mut a = BitRange::new();
        a.insert_impl(0, 1);
        a.insert_impl(9, 1);
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
        let mut a = BitRange::new();
        a.insert_impl(5, 1);
        a.insert_impl(9, 1);
        validate_iter_inverse_ranges(&a, 0, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 0, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 0, 7, 6, 2);
        validate_iter_inverse_ranges(&a, 0, 9, 8, 2);
        validate_iter_inverse_ranges(&a, 0, 10, 8, 2);
        validate_iter_inverse_ranges(&a, 0, 11, 9, 3);
    }
}
