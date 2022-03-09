//! This module provides common zcache structures that are collected by
//! the **zettacache** runtime and consumed by **zcache** subcommands.
//! These structures on the zettacache side are serialized and then deserialized
//! by the zettacache subcommands.

use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize)]
pub struct DeviceEntry {
    pub name: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceEntry>,
}
