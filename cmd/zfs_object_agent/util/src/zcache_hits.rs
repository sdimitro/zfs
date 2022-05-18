//! This module provides common zcache structures that are collected by the **zettacache**
//! runtime and consumed by **zcache hits** subcommand. These structures are serialized by
//! the agent (zettacache) and deserialized by the zcache subcommands.

use std::cmp::min;
use std::time::SystemTime;

use num_traits::ToPrimitive;
use serde::Deserialize;
use serde::Serialize;

// Note: This is essentially SizeHistogramPhys with live and ghost merged into combined_histogram
#[derive(Debug, Serialize, Deserialize)]
pub struct ReportHitsResponse {
    pub started: SystemTime,
    pub lookups: u64,
    pub real_hits: u64, // live, not ghost hits
    pub cache_capacity: u64,
    pub bucket_size: u64,
    #[serde(default)] // serde_nvlist omits empty Vec's
    pub combined_histogram: Vec<u64>, // includes real and ghost
}

impl ReportHitsResponse {
    /// Resample a histogram to produce a new histogram with the requested number of buckets.
    /// This works by dividing each sample in the original histogram into "samples" chunks and
    /// then adding the number of samples for the physical cache in the original histogram of
    /// these chunks together for each bucket in the new histogram.
    pub fn resampled(&self, quantiles: usize) -> Self {
        if quantiles == 0 || self.combined_histogram.is_empty() {
            return Self {
                bucket_size: 0,
                combined_histogram: Vec::new(),
                ..*self
            };
        }

        let sub_samples_per_resample = self.combined_histogram.len();
        let mut sample_iter = self.combined_histogram.iter();
        let mut sub_sample_value = 0.0;
        let mut samples_left = 0;
        let mut combined_histogram: Vec<u64> = Vec::new();
        'outer: loop {
            let mut accumulated_value = 0.0;
            let mut needed_samples = sub_samples_per_resample;
            while needed_samples > 0 {
                if samples_left == 0 {
                    match sample_iter.next() {
                        Some(sample) => {
                            sub_sample_value = *sample as f64 / quantiles as f64;
                        }
                        None => break 'outer,
                    }
                    samples_left = quantiles;
                }
                let samples_to_add = min(needed_samples, samples_left);
                accumulated_value += samples_to_add as f64 * sub_sample_value;
                needed_samples -= samples_to_add;
                samples_left -= samples_to_add;
            }
            combined_histogram.push(accumulated_value.round().to_u64().unwrap());
        }
        Self {
            bucket_size: self.bucket_size * self.combined_histogram.len() as u64
                / combined_histogram.len() as u64,
            combined_histogram,
            ..*self
        }
    }
}
