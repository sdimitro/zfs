#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]

mod alloc;
pub mod async_cache;
mod binaryindextree;
mod bitrange;
mod btreemap_ext;
pub mod concurrent_batch;
mod credentials;
mod die;
mod from64;
pub mod lazy_static_ptr;
mod lock_set;
mod logging;
pub mod measure;
pub mod message;
mod mutex_ext;
mod nicenum;
mod range_tree;
pub mod serde;
pub mod tunable;
pub mod unordered;
mod vec_ext;
pub mod watch_once;
pub mod write_stdout;
mod zcache_devices;
pub mod zettacache_stats;

pub use alloc::with_alloctag;
pub use alloc::with_alloctag_hf;
pub use alloc::TrackingAllocator;
pub use alloc::ALLOCATOR_PRINT_MIN_ALLOCS;
pub use alloc::ALLOCATOR_PRINT_MIN_BYTES;

pub use binaryindextree::BinaryIndexTree;
pub use bitrange::BitRange;
pub use btreemap_ext::iter_wrapping;
pub use credentials::ResilientCredentialsProvider;
pub use die::maybe_die_with;
pub use from64::From64;
pub use lock_set::LockSet;
pub use lock_set::LockedItem;
pub use logging::log;
pub use logging::register_siguser1_to_dump_tracing;
pub use logging::setup_logging;
pub use logging::SUPER_EXPENSIVE_TRACE;
pub use mutex_ext::lock_non_send;
pub use nicenum::nice_number_count;
pub use nicenum::nice_number_time;
pub use nicenum::nice_p2size;
pub use range_tree::RangeTree;
pub use vec_ext::read_buf_exact_len;
pub use vec_ext::tersevec;
pub use vec_ext::AlignedBytes;
pub use vec_ext::AlignedVec;
pub use vec_ext::VecMap;
pub use write_stdout::flush_stdout;
pub use zcache_devices::DeviceEntry;
pub use zcache_devices::DeviceList;
