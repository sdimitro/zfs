use bytesize::ByteSize;

use crate::tunable::ByteSize32;

/// Conversions that are safe assuming that we are on LP64 (usize == u64)
pub trait From64<A> {
    fn from64(a: A) -> Self;
}

impl From64<u64> for usize {
    fn from64(a: u64) -> usize {
        a.try_into().unwrap()
    }
}

pub trait AsUsize {
    fn as_usize(&self) -> usize;
}

impl AsUsize for ByteSize {
    fn as_usize(&self) -> usize {
        usize::from64(self.as_u64())
    }
}

impl AsUsize for ByteSize32 {
    fn as_usize(&self) -> usize {
        self.as_u32() as usize
    }
}

impl AsUsize for u64 {
    fn as_usize(&self) -> usize {
        usize::from64(*self)
    }
}
