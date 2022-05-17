//! This module provides common zcache structures that are collected by
//! the **zettacache** runtime and consumed by **zcache** subcommands.
//! These structures on the zettacache side are serialized and then deserialized
//! by the zettacache subcommands.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize)]
pub struct DeviceEntry {
    pub name: PathBuf,
    pub size: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceEntry>,
}
