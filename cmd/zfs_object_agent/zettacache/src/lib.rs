#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]

mod atime_histogram;
pub mod base_types;
mod block_access;
mod block_allocator;
mod block_based_log;
mod extent_allocator;
mod features;
mod index;
mod size_histogram;
mod space_map;
mod superblock;
mod zcachedb;
mod zettacache;

pub use crate::zettacache::InsertSource;
pub use crate::zettacache::LookupOperation;
pub use crate::zettacache::LookupResponse;
pub use crate::zettacache::LookupSource;
pub use crate::zettacache::ZettaCache;
pub use zcachedb::DumpSlabsOptions;
pub use zcachedb::DumpStructuresOptions;
pub use zcachedb::ZettaCacheDBCommand;
