use more_asserts::assert_gt;
use roaring::bitmap::Iter;
use roaring::RoaringBitmap;
use std::iter::Fuse;

/// An iterator over the ranges of set bits on a bitmap
///
/// This `struct` is created by the [`iter_ranges`] method on
/// [`BitmapRangeIterator`]. See its documentation for more.
///
/// [`iter_ranges`]: BitmapRangeIterator::iter_ranges
pub struct BitmapRangeIter<'a> {
    bitmap_iter: Fuse<Iter<'a>>,
    current_range: Option<(u32, u32)>,
}

impl<'a> BitmapRangeIter<'a> {
    fn new(bitmap: &RoaringBitmap) -> BitmapRangeIter {
        BitmapRangeIter {
            bitmap_iter: bitmap.iter().fuse(),
            current_range: None,
        }
    }
}

impl<'a> Iterator for BitmapRangeIter<'a> {
    // Starting and ending indices representing a range of a set bits in a
    // bitmap.  Note that the limits of the range are inclusive! (e.g. [start,
    // end] as opposed to [start, end)).
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        loop {
            match self.bitmap_iter.next() {
                Some(slot) => match self.current_range {
                    Some((first, last)) if slot == (last + 1) => {
                        self.current_range = Some((first, slot));
                    }
                    Some((first, last)) => {
                        self.current_range = Some((slot, slot));
                        return Some((first, last));
                    }
                    None => {
                        self.current_range = Some((slot, slot));
                    }
                },
                None => return self.current_range.take(),
            }
        }
    }
}

/// An iterator over the ranges of non-set bits on a bitmap
///
/// This `struct` is created by the [`iter_inverse_ranges`] method on
/// [`BitmapInverseRangeIterator`]. See its documentation for more.
///
/// [`iter_inverse_ranges`]: BitmapRangeIterator::iter_inverese_ranges
pub struct BitmapInverseRangeIter<'a> {
    bitmap_iter: Fuse<Iter<'a>>,
    end_slot: u32,
    cursor: u32,
}

impl<'a> BitmapInverseRangeIter<'a> {
    fn new(bitmap: &RoaringBitmap, end_slot: u32) -> BitmapInverseRangeIter {
        BitmapInverseRangeIter {
            bitmap_iter: bitmap.iter().fuse(),
            end_slot,
            cursor: 0,
        }
    }
}

impl<'a> Iterator for BitmapInverseRangeIter<'a> {
    // Starting and ending indices representing a range of non-set bits in a
    // bitmap.  Note that the limits of the range are inclusive! (e.g. [start,
    // end] as opposed to [start, end)).
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        loop {
            if self.cursor == self.end_slot {
                return None;
            }
            let c = self.cursor;
            match self.bitmap_iter.next() {
                Some(slot) => {
                    if slot == c && slot < self.end_slot {
                        self.cursor = slot + 1;
                    } else if slot >= self.end_slot {
                        self.cursor = self.end_slot;
                        return Some((c, self.end_slot - 1));
                    } else {
                        assert_gt!(slot, c);
                        self.cursor = slot + 1;
                        return Some((c, slot - 1));
                    }
                }
                None => {
                    self.cursor = self.end_slot;
                    return Some((c, self.end_slot - 1));
                }
            }
        }
    }
}

/// An interface that provides iterators operating on ranges of a bitmap.
///
/// Currently the only implementation of this interface is Roaring Bitmaps
/// (<https://docs.rs/roaring/0.7.0/roaring/bitmap/struct.RoaringBitmap.html>).
pub trait BitmapRangeIterator {
    /// Gets an iterator over the ranges of set bits in a bitmap.
    ///
    /// NOTE: The ranges returned are inclusive ranges (e.g. [start, end])
    /// where the start and end indeces are bundled in a tuple.
    fn iter_ranges(&self) -> BitmapRangeIter;

    /// Gets an iterator over the ranges of unset bits in a bitmap.
    ///
    /// NOTE: The ranges returned are inclusive ranges (e.g. [start, end])
    /// where the start and end indeces are bundled in a tuple.
    fn iter_inverse_ranges(&self, end: u32) -> BitmapInverseRangeIter;
}

impl BitmapRangeIterator for RoaringBitmap {
    fn iter_ranges(&self) -> BitmapRangeIter {
        BitmapRangeIter::new(self)
    }

    fn iter_inverse_ranges(&self, end: u32) -> BitmapInverseRangeIter {
        BitmapInverseRangeIter::new(self, end)
    }
}

#[cfg(test)]
mod test_iter_inverse_ranges {
    use super::*;
    use more_asserts::assert_ge;

    fn validate_iter_inverse_ranges(
        a: &RoaringBitmap,
        end_slot: u32,
        expected_nslots: u32,
        expected_nsegments: u32,
    ) {
        let mut total_slots = 0;
        let mut total_segments = 0;
        for (first, last) in a.iter_inverse_ranges(end_slot) {
            assert_ge!(last, first);
            for slot in first..last + 1 {
                assert!(!a.contains(slot));
            }
            total_slots += last - first + 1;
            total_segments += 1;
        }
        assert_eq!(expected_nslots, total_slots);
        assert_eq!(expected_nsegments, total_segments);
    }

    #[test]
    fn test_empty() {
        let a = RoaringBitmap::new();
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 10, 10, 1);
    }

    #[test]
    fn test_single_start_slot() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 10, 9, 1);
    }

    #[test]
    fn test_single_start_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 1, 1);
        validate_iter_inverse_ranges(&a, 10, 8, 1);
    }

    #[test]
    fn test_single_start_range_2() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..3);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 0, 0);
        validate_iter_inverse_ranges(&a, 10, 7, 1);
    }

    #[test]
    fn test_single_middle_slot() {
        let mut a = RoaringBitmap::new();
        a.insert(5);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 6, 2);
        validate_iter_inverse_ranges(&a, 8, 7, 2);
        validate_iter_inverse_ranges(&a, 9, 8, 2);
        validate_iter_inverse_ranges(&a, 10, 9, 2);
    }

    #[test]
    fn test_single_middle_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..7);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 8, 6, 2);
        validate_iter_inverse_ranges(&a, 9, 7, 2);
        validate_iter_inverse_ranges(&a, 10, 8, 2);
    }

    #[test]
    fn test_single_middle_range_2() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..8);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 8, 5, 1);
        validate_iter_inverse_ranges(&a, 9, 6, 2);
        validate_iter_inverse_ranges(&a, 10, 7, 2);
    }

    #[test]
    fn test_two_slots_start_end() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 4, 3, 1);
        validate_iter_inverse_ranges(&a, 5, 4, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 6, 1);
        validate_iter_inverse_ranges(&a, 8, 7, 1);
        validate_iter_inverse_ranges(&a, 9, 8, 1);
        validate_iter_inverse_ranges(&a, 10, 9, 1);
        validate_iter_inverse_ranges(&a, 11, 9, 1);
        validate_iter_inverse_ranges(&a, 12, 10, 2);
    }

    #[test]
    fn test_start_slot_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 4, 3, 1);
        validate_iter_inverse_ranges(&a, 5, 4, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 6, 1);
        validate_iter_inverse_ranges(&a, 8, 7, 1);
        validate_iter_inverse_ranges(&a, 9, 8, 1);
        validate_iter_inverse_ranges(&a, 10, 9, 1);
        validate_iter_inverse_ranges(&a, 11, 9, 1);
        validate_iter_inverse_ranges(&a, 12, 9, 1);
        validate_iter_inverse_ranges(&a, 13, 10, 2);
    }

    #[test]
    fn test_start_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 1, 1);
        validate_iter_inverse_ranges(&a, 4, 2, 1);
        validate_iter_inverse_ranges(&a, 5, 3, 1);
        validate_iter_inverse_ranges(&a, 6, 4, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 8, 6, 1);
        validate_iter_inverse_ranges(&a, 9, 7, 1);
        validate_iter_inverse_ranges(&a, 10, 8, 1);
        validate_iter_inverse_ranges(&a, 11, 8, 1);
        validate_iter_inverse_ranges(&a, 12, 9, 2);
    }

    #[test]
    fn test_start_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 1, 1);
        validate_iter_inverse_ranges(&a, 4, 2, 1);
        validate_iter_inverse_ranges(&a, 5, 3, 1);
        validate_iter_inverse_ranges(&a, 6, 4, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 8, 6, 1);
        validate_iter_inverse_ranges(&a, 9, 7, 1);
        validate_iter_inverse_ranges(&a, 10, 8, 1);
        validate_iter_inverse_ranges(&a, 11, 8, 1);
        validate_iter_inverse_ranges(&a, 12, 8, 1);
        validate_iter_inverse_ranges(&a, 13, 9, 2);
    }

    #[test]
    fn test_two_slots_middle_end() {
        let mut a = RoaringBitmap::new();
        a.insert(5);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 6, 2);
        validate_iter_inverse_ranges(&a, 8, 7, 2);
        validate_iter_inverse_ranges(&a, 9, 8, 2);
        validate_iter_inverse_ranges(&a, 10, 9, 2);
        validate_iter_inverse_ranges(&a, 11, 9, 2);
        validate_iter_inverse_ranges(&a, 12, 10, 3);
        validate_iter_inverse_ranges(&a, 13, 11, 3);
    }

    #[test]
    fn test_middle_slot_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(5);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 6, 2);
        validate_iter_inverse_ranges(&a, 8, 7, 2);
        validate_iter_inverse_ranges(&a, 9, 8, 2);
        validate_iter_inverse_ranges(&a, 10, 9, 2);
        validate_iter_inverse_ranges(&a, 11, 9, 2);
        validate_iter_inverse_ranges(&a, 12, 9, 2);
        validate_iter_inverse_ranges(&a, 13, 10, 3);
    }

    #[test]
    fn test_middle_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..7);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 8, 6, 2);
        validate_iter_inverse_ranges(&a, 9, 7, 2);
        validate_iter_inverse_ranges(&a, 10, 8, 2);
        validate_iter_inverse_ranges(&a, 11, 8, 2);
        validate_iter_inverse_ranges(&a, 12, 9, 3);
        validate_iter_inverse_ranges(&a, 13, 10, 3);
    }

    #[test]
    fn test_middle_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..7);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 1, 1);
        validate_iter_inverse_ranges(&a, 2, 2, 1);
        validate_iter_inverse_ranges(&a, 3, 3, 1);
        validate_iter_inverse_ranges(&a, 4, 4, 1);
        validate_iter_inverse_ranges(&a, 5, 5, 1);
        validate_iter_inverse_ranges(&a, 6, 5, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 1);
        validate_iter_inverse_ranges(&a, 8, 6, 2);
        validate_iter_inverse_ranges(&a, 9, 7, 2);
        validate_iter_inverse_ranges(&a, 10, 8, 2);
        validate_iter_inverse_ranges(&a, 11, 8, 2);
        validate_iter_inverse_ranges(&a, 12, 8, 2);
        validate_iter_inverse_ranges(&a, 13, 9, 3);
    }

    #[test]
    fn test_three_slots_start_middle_end() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert(5);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 4, 3, 1);
        validate_iter_inverse_ranges(&a, 5, 4, 1);
        validate_iter_inverse_ranges(&a, 6, 4, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 2);
        validate_iter_inverse_ranges(&a, 8, 6, 2);
        validate_iter_inverse_ranges(&a, 9, 7, 2);
        validate_iter_inverse_ranges(&a, 10, 8, 2);
        validate_iter_inverse_ranges(&a, 11, 8, 2);
        validate_iter_inverse_ranges(&a, 12, 9, 3);
        validate_iter_inverse_ranges(&a, 13, 10, 3);
    }

    #[test]
    fn test_start_range_middle_slot_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert(5);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 1, 1);
        validate_iter_inverse_ranges(&a, 4, 2, 1);
        validate_iter_inverse_ranges(&a, 5, 3, 1);
        validate_iter_inverse_ranges(&a, 6, 3, 1);
        validate_iter_inverse_ranges(&a, 7, 4, 2);
        validate_iter_inverse_ranges(&a, 8, 5, 2);
        validate_iter_inverse_ranges(&a, 9, 6, 2);
        validate_iter_inverse_ranges(&a, 10, 7, 2);
        validate_iter_inverse_ranges(&a, 11, 7, 2);
        validate_iter_inverse_ranges(&a, 12, 8, 3);
        validate_iter_inverse_ranges(&a, 13, 9, 3);
    }

    #[test]
    fn test_start_slot_middle_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert_range(5..7);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 4, 3, 1);
        validate_iter_inverse_ranges(&a, 5, 4, 1);
        validate_iter_inverse_ranges(&a, 6, 4, 1);
        validate_iter_inverse_ranges(&a, 7, 4, 1);
        validate_iter_inverse_ranges(&a, 8, 5, 2);
        validate_iter_inverse_ranges(&a, 9, 6, 2);
        validate_iter_inverse_ranges(&a, 10, 7, 2);
        validate_iter_inverse_ranges(&a, 11, 7, 2);
        validate_iter_inverse_ranges(&a, 12, 8, 3);
        validate_iter_inverse_ranges(&a, 13, 9, 3);
    }

    #[test]
    fn test_start_slot_middle_slot_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert(5);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 4, 3, 1);
        validate_iter_inverse_ranges(&a, 5, 4, 1);
        validate_iter_inverse_ranges(&a, 6, 4, 1);
        validate_iter_inverse_ranges(&a, 7, 5, 2);
        validate_iter_inverse_ranges(&a, 8, 6, 2);
        validate_iter_inverse_ranges(&a, 9, 7, 2);
        validate_iter_inverse_ranges(&a, 10, 8, 2);
        validate_iter_inverse_ranges(&a, 11, 8, 2);
        validate_iter_inverse_ranges(&a, 12, 8, 2);
        validate_iter_inverse_ranges(&a, 13, 9, 3);
    }

    #[test]
    fn test_start_slot_middle_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert_range(5..7);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 1, 1);
        validate_iter_inverse_ranges(&a, 3, 2, 1);
        validate_iter_inverse_ranges(&a, 4, 3, 1);
        validate_iter_inverse_ranges(&a, 5, 4, 1);
        validate_iter_inverse_ranges(&a, 6, 4, 1);
        validate_iter_inverse_ranges(&a, 7, 4, 1);
        validate_iter_inverse_ranges(&a, 8, 5, 2);
        validate_iter_inverse_ranges(&a, 9, 6, 2);
        validate_iter_inverse_ranges(&a, 10, 7, 2);
        validate_iter_inverse_ranges(&a, 11, 7, 2);
        validate_iter_inverse_ranges(&a, 12, 7, 2);
        validate_iter_inverse_ranges(&a, 13, 8, 3);
    }

    #[test]
    fn test_start_range_middle_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert_range(5..7);
        a.insert(10);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 1, 1);
        validate_iter_inverse_ranges(&a, 4, 2, 1);
        validate_iter_inverse_ranges(&a, 5, 3, 1);
        validate_iter_inverse_ranges(&a, 6, 3, 1);
        validate_iter_inverse_ranges(&a, 7, 3, 1);
        validate_iter_inverse_ranges(&a, 8, 4, 2);
        validate_iter_inverse_ranges(&a, 9, 5, 2);
        validate_iter_inverse_ranges(&a, 10, 6, 2);
        validate_iter_inverse_ranges(&a, 11, 6, 2);
        validate_iter_inverse_ranges(&a, 12, 7, 3);
        validate_iter_inverse_ranges(&a, 13, 8, 3);
    }

    #[test]
    fn test_start_range_middle_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert_range(5..7);
        a.insert_range(10..12);
        validate_iter_inverse_ranges(&a, 0, 0, 0);
        validate_iter_inverse_ranges(&a, 1, 0, 0);
        validate_iter_inverse_ranges(&a, 2, 0, 0);
        validate_iter_inverse_ranges(&a, 3, 1, 1);
        validate_iter_inverse_ranges(&a, 4, 2, 1);
        validate_iter_inverse_ranges(&a, 5, 3, 1);
        validate_iter_inverse_ranges(&a, 6, 3, 1);
        validate_iter_inverse_ranges(&a, 7, 3, 1);
        validate_iter_inverse_ranges(&a, 8, 4, 2);
        validate_iter_inverse_ranges(&a, 9, 5, 2);
        validate_iter_inverse_ranges(&a, 10, 6, 2);
        validate_iter_inverse_ranges(&a, 11, 6, 2);
        validate_iter_inverse_ranges(&a, 12, 6, 2);
        validate_iter_inverse_ranges(&a, 13, 7, 3);
    }
}

#[cfg(test)]
mod test_iter_ranges {
    use super::*;
    use more_asserts::assert_ge;

    fn validate_iter_ranges(a: &RoaringBitmap, expected_nsegs: u32) {
        let mut total_slots = 0;
        let mut total_segments = 0;
        for (first, last) in a.iter_ranges() {
            assert_ge!(last, first);
            for slot in first..last + 1 {
                assert!(a.contains(slot));
            }
            total_slots += last - first + 1;
            total_segments += 1;
        }
        assert_eq!(u64::from(total_slots), a.len());
        assert_eq!(total_segments, expected_nsegs);
    }

    #[test]
    fn test_empty() {
        let a = RoaringBitmap::new();
        validate_iter_ranges(&a, 0);
    }

    #[test]
    fn test_single_start_slot() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        validate_iter_ranges(&a, 1);
    }

    #[test]
    fn test_single_start_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        validate_iter_ranges(&a, 1);
    }

    #[test]
    fn test_single_start_range_2() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..3);
        validate_iter_ranges(&a, 1);
    }

    #[test]
    fn test_single_middle_slot() {
        let mut a = RoaringBitmap::new();
        a.insert(5);
        validate_iter_ranges(&a, 1);
    }

    #[test]
    fn test_single_middle_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..7);
        validate_iter_ranges(&a, 1);
    }

    #[test]
    fn test_single_middle_range_2() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..8);
        validate_iter_ranges(&a, 1);
    }

    #[test]
    fn test_two_slots_start_end() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert(10);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_start_slot_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_start_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert(10);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_start_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_two_slots_middle_end() {
        let mut a = RoaringBitmap::new();
        a.insert(5);
        a.insert(10);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_middle_slot_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(5);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_middle_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..7);
        a.insert(10);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_middle_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(5..7);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 2);
    }

    #[test]
    fn test_three_slots_start_middle_end() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert(5);
        a.insert(10);
        validate_iter_ranges(&a, 3);
    }

    #[test]
    fn test_start_range_middle_slot_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert(5);
        a.insert(10);
        validate_iter_ranges(&a, 3);
    }

    #[test]
    fn test_start_slot_middle_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert_range(5..7);
        a.insert(10);
        validate_iter_ranges(&a, 3);
    }

    #[test]
    fn test_start_slot_middle_slot_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert(5);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 3);
    }

    #[test]
    fn test_start_slot_middle_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert(0);
        a.insert_range(5..7);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 3);
    }

    #[test]
    fn test_start_range_middle_range_end_slot() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert_range(5..7);
        a.insert(10);
        validate_iter_ranges(&a, 3);
    }

    #[test]
    fn test_start_range_middle_range_end_range() {
        let mut a = RoaringBitmap::new();
        a.insert_range(0..2);
        a.insert_range(5..7);
        a.insert_range(10..12);
        validate_iter_ranges(&a, 3);
    }
}
