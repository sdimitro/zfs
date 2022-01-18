#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]

mod bitmap_range_iterator;
mod btreemap_ext;
mod credentials;
mod die;
mod from64;
mod lock_set;
mod logging;
mod mutex_ext;
mod nicenum;
mod range_tree;
mod tunable;
mod vec_ext;
mod zcache_devices;
pub mod zettacache_stats;

pub use bitmap_range_iterator::BitmapRangeIterator;
pub use btreemap_ext::iter_wrapping;
pub use credentials::ResilientCredentialsProvider;
pub use die::maybe_die_with;
pub use from64::From64;
pub use lock_set::LockSet;
pub use lock_set::LockedItem;
pub use logging::setup_logging;
pub use logging::SUPER_EXPENSIVE_TRACE;
pub use mutex_ext::MutexExt;
pub use nicenum::{nice_number_count, nice_number_time, nice_p2size};
pub use range_tree::RangeTree;
pub use tunable::get_tunable;
pub use tunable::read_tunable_config;
pub use vec_ext::AlignedBytes;
pub use vec_ext::AlignedVec;
pub use vec_ext::TerseVec;
pub use zcache_devices::{DeviceEntry, DeviceList};
