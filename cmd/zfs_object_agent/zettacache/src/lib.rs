#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]

mod atime_histogram;
pub mod base_types;
mod block_access;
mod block_allocator;
mod block_based_log;
mod checkpoint;
mod features;
mod index;
mod open;
mod pool_id;
mod size_histogram;
mod slab_allocator;
mod space_map;
mod superblock;
mod zcachedb;
mod zettacache;

pub use zcachedb::DumpSlabsOptions;
pub use zcachedb::DumpStructuresOptions;
pub use zcachedb::ZettaCacheDBCommand;

pub use crate::open::CacheOpenMode;
pub use crate::zettacache::InsertSource;
pub use crate::zettacache::LockedKey;
pub use crate::zettacache::LookupResponse;
pub use crate::zettacache::ZettaCache;
