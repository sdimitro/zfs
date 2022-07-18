//! This module provides common zcache structures that are collected by
//! the **zettacache** runtime and consumed by **zcache** subcommands.
//! These structures on the zettacache side are serialized and then deserialized
//! by the zettacache subcommands.

use chrono::DateTime;
use chrono::Local;
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
    // We allow one device to be actively removed at a time, is it this one?
    pub currently_removing_device: bool,

    // Space that needs to be evicted in order for the evacuation to start.
    pub space_left_to_evict: u64,

    // Allocated space in the removing device that needs evacuation at the beginning of the
    // removal.
    pub total_space_to_evacuate: u64,

    // Space that needs evacuation currently.
    pub space_left_to_evacuate: u64,

    // The time that we started evacuating user data away from that device. Set to None before
    // that point.
    pub start_time: Option<DateTime<Local>>,
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
