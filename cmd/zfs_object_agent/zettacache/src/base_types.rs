use more_asserts::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt::*;
use std::ops::Add;
use std::ops::Sub;

/*
 * Things that are stored on disk.
 */
pub trait OnDisk: Serialize + DeserializeOwned {}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct PoolGuid(pub u64);
impl OnDisk for PoolGuid {}
impl Display for PoolGuid {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{:020}", self.0)
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct BlockId(pub u64);
impl OnDisk for BlockId {}
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

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct DiskId(pub u16);

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
#[repr(packed)]
pub struct DiskLocation {
    disk: DiskId,
    offset: u64,
}
impl DiskLocation {
    pub fn new(disk: DiskId, offset: u64) -> Self {
        Self { disk, offset }
    }
    pub fn disk(&self) -> DiskId {
        self.disk
    }
    pub fn offset(&self) -> u64 {
        self.offset
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
}

/// This allows Extents to be compared by their `location`s, ignoring the `size`s.
impl Borrow<DiskLocation> for Extent {
    fn borrow(&self) -> &DiskLocation {
        &self.location
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct CheckpointId(pub u64);
impl CheckpointId {
    pub fn next(&self) -> CheckpointId {
        CheckpointId(self.0 + 1)
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
