//! This module provides common zcache structures that are collected by the **zettacache**
//! runtime and consumed by **zcache hits** subcommand. These structures are serialized by
//! the agent (zettacache) and deserialized by the zcache subcommands.

use std::time::SystemTime;

use serde::Deserialize;
use serde::Serialize;

// Note: This is essentially SizeHistogramPhys with live and ghost merged into combined_histogram
#[derive(Debug, Serialize, Deserialize)]
pub struct ReportHitsResponse {
    pub started: SystemTime,
    pub cache_lookups: u64,
    pub cache_capacity: u64,
    pub bucket_size: u64,
    #[serde(default)] // serde_nvlist omits empty Vec's
    pub combined_histogram: Vec<u64>,
}
