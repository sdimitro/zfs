#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_possible_truncation)]
#![warn(clippy::cast_possible_wrap)]
#![warn(clippy::cast_sign_loss)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]

pub mod access_stats;
pub mod base_types;
pub mod data_object;
pub mod debug;
mod features;
mod heartbeat;
pub mod init;
pub mod object_access;
mod object_based_log;
mod object_block_map;
mod object_deleter;
mod pool;
mod pool_destroy;
mod public_connection;
mod root_connection;
mod server;
pub mod test_connectivity;

pub use pool::Pool;
