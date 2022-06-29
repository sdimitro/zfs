//! This module provides common zcache structures that are collected by
//! the **zettacache** runtime and consumed by **zcache** subcommands.
//! These structures on the zettacache side are serialized and then deserialized
//! by the zettacache subcommands.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IndexStatus {
    pub bytes: u64,
    pub entries: u64,
    pub pending_changes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub path: PathBuf,
    pub canonical_path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ZcacheStatus {
    pub index: IndexStatus,
    pub devices: Vec<DeviceStatus>,
}
