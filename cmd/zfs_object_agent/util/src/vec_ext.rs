use std::cmp::min;
use std::fmt::Formatter;
use std::fmt::Result;
use std::marker::PhantomData;
use std::mem;
use std::ops::Bound;
use std::ops::Deref;
use std::ops::Range;
use std::ops::RangeBounds;

use bytes::buf::UninitSlice;
use bytes::BufMut;
use bytes::Bytes;
use derivative::Derivative;
use more_asserts::*;
use tokio::io;
use tokio::io::AsyncReadExt;

/// # Examples:
/// ```
/// use derivative::Derivative;
/// #[derive(Derivative)]
/// #[derivative(Debug)]
/// struct Foo {
///     #[derivative(Debug(format_with = "util::tersevec"))]
///     member: Vec<u64>
/// }
/// ```
pub fn tersevec<E>(vec: &[E], fmt: &mut Formatter) -> Result {
    fmt.write_fmt(format_args!("[...{} elements...]", vec.len()))
}

#[derive(Debug, Clone)]
pub struct AlignedBytes {
    alignment: usize,
    bytes: Bytes,
}

impl AlignedBytes {
    pub fn copy_from_slice(slice: &[u8], alignment: usize) -> Self {
        let mut vec = AlignedVec::with_capacity(slice.len(), alignment);
        vec.extend_from_slice(slice);
        vec.into()
    }

    pub fn alignment(&self) -> usize {
        self.alignment
    }

    pub fn slice_ref(&self, subset: &[u8]) -> Self {
        assert!(subset.len() % self.alignment == 0);
        let offset = subset.as_ptr() as usize - self.bytes.as_ref().as_ptr() as usize;
        assert!(offset % self.alignment == 0);
        let sub_bytes = self.bytes.slice_ref(subset);
        Self {
            alignment: self.alignment,
            bytes: sub_bytes,
        }
    }

    pub fn as_bytes(&self) -> Bytes {
        self.bytes.clone()
    }
}

impl Deref for AlignedBytes {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl From<Bytes> for AlignedBytes {
    fn from(bytes: Bytes) -> Self {
        // determine pointer alignment; we only care about valid sector sizes so we check 512 -> 16K
        let mut alignment = 1;
        for i in (9..15).rev() {
            if bytes.as_ptr().align_offset(1 << i) == 0 {
                alignment = 1 << i;
                break;
            }
        }
        AlignedBytes { alignment, bytes }
    }
}

impl From<AlignedBytes> for Bytes {
    fn from(aligned_bytes: AlignedBytes) -> Self {
        aligned_bytes.bytes
    }
}

impl From<AlignedVec> for AlignedBytes {
    fn from(mut aligned_vec: AlignedVec) -> Self {
        aligned_vec.verify();
        let ptr = aligned_vec.vec.as_ptr();
        // resize so that there is no spare capacity, so that converting to Bytes will not
        // reallocate
        let raw_len = aligned_vec.vec.len();
        aligned_vec.vec.resize(aligned_vec.vec.capacity(), 0);
        assert_eq!(aligned_vec.vec.as_ptr(), ptr);
        let unaligned_bytes: Bytes = aligned_vec.vec.into();
        assert_eq!(unaligned_bytes.as_ptr(), ptr);
        let bytes = unaligned_bytes.slice(aligned_vec.pad..raw_len);
        assert_eq!(
            bytes.as_ptr().align_offset(aligned_vec.alignment),
            0,
            "pointer {:?}+{} is not {}-aligned",
            unaligned_bytes.as_ptr(),
            aligned_vec.pad,
            aligned_vec.alignment
        );
        AlignedBytes {
            alignment: aligned_vec.alignment,
            bytes,
        }
    }
}

#[derive(Debug)]
pub struct AlignedVec {
    alignment: usize,
    vec: Vec<u8>,
    pad: usize,
}

impl AlignedVec {
    pub fn with_capacity(capacity: usize, alignment: usize) -> Self {
        assert_ne!(alignment, 0);
        let mut vec: Vec<u8> = Vec::with_capacity(capacity + alignment);
        let pad = vec.as_ptr().align_offset(alignment);
        assert_lt!(pad, alignment);
        vec.resize(pad, 0);
        let aligned_vec = AlignedVec {
            alignment,
            vec,
            pad,
        };
        aligned_vec.verify();
        aligned_vec
    }

    fn verify(&self) {
        assert_eq!(self.as_ptr().align_offset(self.alignment), 0);
    }

    pub fn extend_from_slice(&mut self, slice: &[u8]) {
        // We can't allow the vec to be resized, as that could violate the alignment constraint.
        assert_le!(slice.len(), self.vec.capacity() - self.vec.len());
        self.vec.extend_from_slice(slice);
        self.verify();
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.vec[self.pad..]
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.vec[self.pad..]
    }

    pub fn len(&self) -> usize {
        self.vec.len() - self.pad
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn unused_capacity(&self) -> usize {
        self.vec.capacity() - self.vec.len()
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.vec[self.pad..].as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.vec[self.pad..].as_mut_ptr()
    }

    /// # Safety
    ///
    /// See `Vec::set_len()`
    /// - `new_len` must be <= the capacity specified by `.with_capacity()`.
    /// - The elements at `old_len..new_len` must be initialized.
    pub unsafe fn set_len(&mut self, new_len: usize) {
        assert_le!(self.pad + new_len, self.vec.capacity());
        self.vec.set_len(self.pad + new_len);
    }
}

unsafe impl BufMut for AlignedVec {
    fn remaining_mut(&self) -> usize {
        self.unused_capacity()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        self.set_len(self.len() + cnt);
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        unsafe {
            UninitSlice::from_raw_parts_mut(
                self.as_mut_ptr().offset(self.len().try_into().unwrap()),
                self.remaining_mut(),
            )
        }
    }
}

// BufMut's typically have "remaining capacity" that is not directly controlled from the consumer
// (e.g. `Vec<u8>` has unlimited remaining, `AlignedVec` has remaining that includes the trailing
// padding).  Therefore we take the `len` parameter and use the `BufMut::limit()` wrapper.
pub async fn read_buf_exact_len<R, B>(reader: &mut R, buf: &mut B, len: usize) -> io::Result<()>
where
    R: AsyncReadExt + Unpin,
    B: BufMut,
{
    let mut buf = buf.limit(len);
    while buf.has_remaining_mut() {
        reader.read_buf(&mut buf).await?;
    }
    Ok(())
}

/// The VecMap provides similar functionality to a BTreeMap, but with a Vec as the underlying
/// data structure.  For good performance, the keys must be dense integers.
#[derive(Debug, Derivative)]
#[derivative(Default(bound = ""))]
pub struct VecMap<K, V> {
    vec: Vec<Option<V>>,
    num_entries: usize,
    phantom: PhantomData<K>,
}

impl<K, V> VecMap<K, V>
where
    K: Into<usize> + Copy,
{
    /// Returns old value (or None if not present)
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let index = key.into();
        if index >= self.vec.len() {
            self.vec.resize_with(index + 1, || None);
        }
        if self.vec[index].is_none() {
            self.num_entries += 1;
        }
        mem::replace(&mut self.vec[index], Some(value))
    }

    pub fn get(&self, key: K) -> Option<&V> {
        let index = key.into();
        if index >= self.vec.len() {
            return None;
        }
        self.vec[index].as_ref()
    }

    pub fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let index = key.into();
        if index >= self.vec.len() {
            return None;
        }
        self.vec[index].as_mut()
    }

    /// Returns old value (or None if not present)
    pub fn remove(&mut self, key: K) -> Option<V> {
        let index = key.into();
        if index >= self.vec.len() {
            return None;
        }
        if self.vec[index].is_some() {
            self.num_entries -= 1;
        }
        self.vec[index].take()
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.vec.iter().filter_map(|v| v.as_ref())
    }

    fn map_range<R: RangeBounds<K>>(&self, range: R) -> Range<usize> {
        let start = match range.start_bound() {
            Bound::Included(&k) => k.into(),
            Bound::Excluded(&k) => k.into() + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&k) => k.into() + 1,
            Bound::Excluded(&k) => k.into(),
            Bound::Unbounded => self.vec.len(),
        };
        min(self.vec.len(), start)..min(self.vec.len(), end)
    }

    pub fn range<R: RangeBounds<K>>(&self, range: R) -> impl Iterator<Item = &V> {
        let range = self.map_range(range);
        self.vec[range].iter().filter_map(|v| v.as_ref())
    }

    pub fn range_mut<R: RangeBounds<K>>(&mut self, range: R) -> impl Iterator<Item = &mut V> {
        let range = self.map_range(range);
        self.vec[range].iter_mut().filter_map(|v| v.as_mut())
    }

    /// Returns the number of elements in the map.
    pub fn len(&self) -> usize {
        self.num_entries
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
