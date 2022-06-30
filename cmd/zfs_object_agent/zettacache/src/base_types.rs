use std::borrow::Borrow;
use std::cmp::min;
use std::fmt::*;
use std::num::NonZeroU64;
use std::num::ParseIntError;
use std::ops::Add;
use std::ops::Sub;
use std::str::FromStr;

use more_asserts::*;
use serde::Deserialize;
use serde::Serialize;
use util::From64;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct PoolGuid(pub u64);
impl Display for PoolGuid {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{:020}", self.0)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct CacheGuid(pub u64);
impl CacheGuid {
    pub fn new() -> Self {
        CacheGuid(rand::random())
    }
}
impl Default for CacheGuid {
    fn default() -> Self {
        CacheGuid::new()
    }
}
impl FromStr for CacheGuid {
    type Err = ParseIntError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        s.parse().map(CacheGuid)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DiskGuid(pub u64);
impl DiskGuid {
    pub fn new() -> Self {
        DiskGuid(rand::random())
    }
}
impl Default for DiskGuid {
    fn default() -> Self {
        DiskGuid::new()
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct BlockId(pub u64);
impl Display for BlockId {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{}", self.0)
    }
}
impl BlockId {
    pub fn next(&self) -> BlockId {
        BlockId(self.0 + 1)
    }
}
impl Sub<BlockId> for BlockId {
    type Output = usize;

    fn sub(self, rhs: BlockId) -> Self::Output {
        usize::from64(self.0 - rhs.0)
    }
}
impl Add<usize> for BlockId {
    type Output = Self;

    fn add(self, rhs: usize) -> Self::Output {
        Self(self.0 + rhs as u64)
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct DiskId(u16);
impl DiskId {
    /// Due to encoding in the DiskLocation, the maximum DiskId + 1 has to fit into 9 bits
    pub const MAX_VALUE: usize = (1 << 9) - 2;
    pub fn new(value: usize) -> Self {
        assert_le!(value, Self::MAX_VALUE);
        DiskId(u16::try_from(value).unwrap())
    }
    pub fn index(self) -> usize {
        self.0 as usize
    }
    pub fn next(&self) -> DiskId {
        DiskId(self.0 + 1)
    }
}
impl From<DiskId> for usize {
    fn from(val: DiskId) -> Self {
        val.index()
    }
}
impl From<usize> for DiskId {
    fn from(val: usize) -> Self {
        DiskId::new(val)
    }
}

#[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
#[repr(packed)]
pub struct DiskLocation(NonZeroU64);
impl DiskLocation {
    /// The low 9 bits of the offset must be zero, and the DiskId + 1 fit in 9 bits
    const BITS: usize = 9;
    const MASK: u64 = (1 << Self::BITS) - 1;
    pub fn new(disk: DiskId, offset: u64) -> Self {
        let disk_raw = u64::from(disk.0) + 1;
        // Check that the high bits of disk_raw are not set, i.e. the maximum DiskId is (2^BITS) - 2
        assert_eq!(disk_raw & Self::MASK, disk_raw);
        // Check that the low bits of the offset are not set, i.e. it is aligned to 512 bytes
        assert_eq!(offset & Self::MASK, 0);
        // Note, we want the DiskId to be stored in the high bits, so that the
        // derived Ord will sort first by DiskId and then by Offset
        // The value is nonzero because we added one to `disk_raw`.
        Self(NonZeroU64::new(disk_raw << (64 - Self::BITS) | offset >> Self::BITS).unwrap())
    }
    pub fn disk(&self) -> DiskId {
        // truncation is not possible, because we've shifted it down to the low 9 bits
        #[allow(clippy::cast_possible_truncation)]
        DiskId(((self.0.get() >> (64 - Self::BITS)) - 1) as u16)
    }
    pub fn offset(&self) -> u64 {
        self.0.get() << Self::BITS
    }
    pub fn from_raw(raw: NonZeroU64) -> Self {
        Self(raw)
    }
    pub fn to_raw(&self) -> NonZeroU64 {
        self.0
    }
}
impl Add<u64> for DiskLocation {
    type Output = DiskLocation;
    fn add(self, rhs: u64) -> Self::Output {
        DiskLocation::new(self.disk(), self.offset() + rhs)
    }
}
impl Add<usize> for DiskLocation {
    type Output = DiskLocation;
    fn add(self, rhs: usize) -> Self::Output {
        self + rhs as u64
    }
}
impl Sub<DiskLocation> for DiskLocation {
    type Output = u64;

    fn sub(self, rhs: DiskLocation) -> Self::Output {
        assert_eq!(self.disk(), rhs.disk());
        self.offset() - rhs.offset()
    }
}
impl Debug for DiskLocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("DiskLocation")
            .field("disk", &self.disk())
            .field("offset", &self.offset())
            .finish()
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct Extent {
    pub location: DiskLocation,
    pub size: u64,
}

impl Extent {
    pub fn new(disk: DiskId, offset: u64, size: u64) -> Extent {
        assert_eq!(offset % 512, 0, "offset {} is not 512-aligned", offset);
        assert_eq!(size % 512, 0, "size {} is not 512-aligned", size);
        Extent {
            location: DiskLocation::new(disk, offset),
            size,
        }
    }

    pub fn range(&self, relative_offset: u64, size: u64) -> Extent {
        assert_ge!(self.size, relative_offset + size);
        Extent {
            location: self.location + relative_offset,
            size,
        }
    }

    /// Trims the beginning off of this extent.
    pub fn trim_start(&self, relative_offset: u64) -> Extent {
        assert_le!(relative_offset, self.size);
        Extent {
            location: self.location + relative_offset,
            size: self.size - relative_offset,
        }
    }

    /// Reduce size to specified value.  If size is larger than the extent's size, this is a
    /// no-op.
    pub fn trim_end(&self, size: u64) -> Extent {
        Extent {
            location: self.location,
            size: min(self.size, size),
        }
    }

    /// returns true if `sub` is entirely contained within this extent
    pub fn contains(&self, sub: &Extent) -> bool {
        sub.location.disk() == self.location.disk()
            && sub.location.offset() >= self.location.offset()
            && sub.location.offset() + sub.size <= self.location.offset() + self.size
    }

    /// returns the sub-range of self that is after `sub`, or None if self does not contain sub
    pub fn after(&self, sub: &Extent) -> Option<Extent> {
        match self.contains(sub) {
            true => Some(Extent::new(
                self.location.disk(),
                sub.location.offset() + sub.size,
                self.location.offset() + self.size - (sub.location.offset() + sub.size),
            )),
            false => None,
        }
    }

    pub fn merge(self, other: Extent) -> Option<Extent> {
        if other.location.disk() == self.location.disk()
            && other.location.offset() == self.location.offset() + self.size
        {
            // other is directly after self
            Some(Self {
                location: self.location,
                size: self.size + other.size,
            })
        } else if other.location.disk() == self.location.disk()
            && self.location.offset() == other.location.offset() + other.size
        {
            // self is directly after other
            Some(Self {
                location: other.location,
                size: other.size + self.size,
            })
        } else {
            None
        }
    }
}

/// This allows Extents to be compared by their `location`s, ignoring the `size`s.
impl Borrow<DiskLocation> for Extent {
    fn borrow(&self) -> &DiskLocation {
        &self.location
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct Atime(pub u32);
impl Atime {
    pub fn next(&self) -> Atime {
        Atime(self.0 + 1)
    }
    pub fn checked_sub(&self, rhs: Self) -> Option<usize> {
        self.0.checked_sub(rhs.0).map(|value| value as usize)
    }
}

impl Sub<Atime> for Atime {
    type Output = usize;
    fn sub(self, rhs: Atime) -> usize {
        (self.0 - rhs.0) as usize
    }
}

impl Add<usize> for Atime {
    type Output = Atime;
    fn add(self, rhs: usize) -> Atime {
        Atime(self.0 + u32::try_from(rhs).unwrap())
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct ReclaimLogId(pub u16);
impl Display for ReclaimLogId {
    // For crash cleanup we want value prefixed with zeroes and there can be up to 2 ^ 16 logs
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{:05}", self.0)
    }
}

impl ReclaimLogId {
    pub fn as_index(self) -> usize {
        usize::from(self.0)
    }
}
