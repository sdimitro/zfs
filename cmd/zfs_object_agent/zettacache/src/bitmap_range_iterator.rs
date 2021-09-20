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
    // Starting and ending indices representing a range on a bitmap.  Note that
    // the limits of the range are inclusive! (e.g. [start, end] as opposed to
    // [start, end)).
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        loop {
            match self.bitmap_iter.next() {
                Some(slot) => match self.current_range {
                    Some((first, last)) if slot == (last + 1) => {
                        self.current_range = Some((first, slot));
                    }
                    Some((first, last)) => {
                        self.current_range = None;
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

/// An interface that provides iterators operating on ranges of a bitmap.
///
/// Currently the only implementation of this interface is Roaring Bitmaps
/// (<https://docs.rs/roaring/0.7.0/roaring/bitmap/struct.RoaringBitmap.html>).
pub trait BitmapRangeIterator {
    /// Gets an iterator over the ranges of set bits on a bitmap.
    ///
    /// NOTE: The ranges returned are inclusive ranges (e.g. [start, end])
    /// where the start and end indeces are bundled in a tuple.
    ///
    /// # Example
    ///
    /// ```
    /// use roaring::RoaringBitmap;
    ///
    /// let mut a = RoaringBitmap::new();
    /// a.insert_range(0..5);
    /// a.insert(7);
    /// a.insert_range(10..12);
    ///
    /// let range_iter = a.iter_ranges();
    /// assert_eq!(range_iter.next(), (0, 4));
    /// assert_eq!(range_iter.next(), (7, 7));
    /// assert_eq!(range_iter.next(), (10, 11));
    /// assert_eq!(range_iter.next(), None);
    /// ```
    fn iter_ranges(&self) -> BitmapRangeIter;
}

impl BitmapRangeIterator for RoaringBitmap {
    fn iter_ranges(&self) -> BitmapRangeIter {
        BitmapRangeIter::new(self)
    }
}
