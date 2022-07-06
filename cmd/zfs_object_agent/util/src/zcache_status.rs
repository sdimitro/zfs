//! This module provides common zcache structures that are collected by
//! the **zettacache** runtime and consumed by **zcache** subcommands.
//! These structures on the zettacache side are serialized and then deserialized
//! by the zettacache subcommands.

use serde::Deserialize;
use serde::Serialize;

use crate::DeviceEntry;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IndexStatus {
    pub bytes: u64,
    pub entries: u64,
    pub pending_changes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RemovalStatus {
    pub space_left_to_evacuate: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub info: DeviceEntry,
    pub removal: Option<RemovalStatus>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ZcacheStatus {
    pub index: IndexStatus,
    pub devices: Vec<DeviceStatus>,
}
