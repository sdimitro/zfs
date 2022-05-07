//! `zcache hits` subcommand

use std::cmp::max;
use std::cmp::Ordering;

use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use clap::Parser;
use num_traits::cast::ToPrimitive;
use serde::Serialize;
use util::message::TYPE_CLEAR_HIT_DATA;
use util::message::TYPE_REPORT_HITS;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
use util::From64;
use util::ReportHitsResponse;

use crate::remote_channel::RemoteChannel;
use crate::remote_channel::RemoteError;
use crate::subcommand::ZcacheSubCommand;

#[derive(Serialize)]
struct HitsBySize {
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    cache_capacity: u64,
    cache_lookups: u64,
    cache_hits: u64,
    bucket_size: u64,
    hits_report: Vec<u64>,
}

impl HitsBySize {
    fn new(report_hits: ReportHitsResponse, quantiles: usize) -> HitsBySize {
        let mut hits_by_size = HitsBySize {
            start_time: report_hits.started.into(),
            end_time: Utc::now(),
            cache_lookups: report_hits.cache_lookups,
            cache_hits: report_hits.combined_histogram.iter().sum(),
            cache_capacity: report_hits.cache_capacity,
            bucket_size: report_hits.bucket_size,
            hits_report: Vec::new(),
        };
        if quantiles == 0 {
            return hits_by_size;
        }

        let raw_length = report_hits.combined_histogram.len() as u64;
        let resampled_report = hits_by_size.resample(quantiles, &report_hits.combined_histogram);
        hits_by_size.bucket_size =
            report_hits.bucket_size * raw_length / resampled_report.len() as u64;

        let mut cumulative_hits = 0;
        for hits in resampled_report.iter() {
            cumulative_hits += hits;
            hits_by_size.hits_report.push(cumulative_hits);
        }

        hits_by_size
    }

    /// Resample a histogram to produce a new histogram with the requested
    /// number of buckets for the capacity portion of the original histogram.
    /// This works by dividing each sample in the original histogram into "samples"
    /// chunks and then adding the number of samples for the physical cache in the
    /// original histogram of these chunks together for each bucket in the new histogram.
    fn resample(&self, samples_in_capacity: usize, histogram: &[u64]) -> Vec<u64> {
        let sub_samples_per_resample = usize::from64(self.cache_capacity / self.bucket_size);
        let mut sample_iter = histogram.iter();
        let mut sub_sample_value = 0.0;
        let mut samples_left = 0;
        let mut resample: Vec<u64> = Vec::new();
        'outer: loop {
            let mut accumulated_value = 0.0;
            let mut needed_samples = sub_samples_per_resample;
            while needed_samples > 0 {
                if samples_left == 0 {
                    match sample_iter.next() {
                        Some(sample) => {
                            sub_sample_value = *sample as f64 / samples_in_capacity as f64;
                        }
                        None => break 'outer,
                    }
                    samples_left = samples_in_capacity;
                }
                let samples_to_add = std::cmp::min(needed_samples, samples_left);
                accumulated_value += samples_to_add as f64 * sub_sample_value;
                needed_samples -= samples_to_add;
                samples_left -= samples_to_add;
            }
            resample.push(accumulated_value.round().to_u64().unwrap());
        }
        resample
    }

    fn print(&self) {
        writeln_stdout!("Data collection started: {}", self.start_time.to_rfc2822());
        writeln_stdout!("Data collection ended: {}", self.end_time.to_rfc2822());
        write_stdout!(
            "Cache Hits by Size ({} lookups with {} hits ",
            self.cache_lookups,
            self.cache_hits
        );
        let hit_percent = if self.cache_lookups == 0 {
            100.0
        } else {
            self.cache_hits as f64 * 100.0 / self.cache_lookups as f64
        };
        writeln_stdout!(
            "({:.1}%) in {} cache)",
            hit_percent,
            nice_p2size(self.cache_capacity)
        );

        if self.hits_report.is_empty() {
            return;
        }

        const HISTOGRAM_WIDTH: usize = 50;
        const PREFIX_WIDTH: usize = 16;
        writeln_stdout!(
            "\n{:>15}{:>2}{:>10}{:>10}{:>10}{:>10}{:>9}",
            "size : %hit",
            0,
            20,
            40,
            60,
            80,
            100
        );
        writeln_stdout!("{0:-<1$}", "-", HISTOGRAM_WIDTH + PREFIX_WIDTH);

        let histogram_length = self.hits_report.len() as u64;
        let histogram_capacity = histogram_length * self.bucket_size;
        let mut cache_size = 0;

        for (index, &cumulative_hits) in self.hits_report.iter().enumerate() {
            cache_size += self.bucket_size;
            // The last bucket may not be the "full" bucket size
            if cache_size > histogram_capacity {
                assert_eq!(
                    index,
                    self.hits_report.len() - 1,
                    "Capacity overflow at histogram index {}",
                    index
                );
                cache_size = histogram_capacity;
            }
            if hit_percent > 99.9 && cache_size > self.cache_capacity {
                break;
            }
            write_stdout!("{: >8} : ", nice_p2size(cache_size));
            if self.cache_hits == 0 {
                writeln_stdout!();
                continue;
            }

            let percent = (cumulative_hits * 100) as f64 / self.cache_lookups as f64;
            let mut stars = if cumulative_hits == 0 {
                // no hits have been seen yet
                write_stdout!("  0% ");
                0
            } else if percent < 1.0 {
                // there are a small number of hits
                write_stdout!(" <1% ");
                1
            } else {
                write_stdout!("{: >3.0}% ", percent);
                max(percent.to_usize().unwrap() * HISTOGRAM_WIDTH / 100, 1)
            };

            let real_stars = hit_percent.to_usize().unwrap() * HISTOGRAM_WIDTH / 100;
            let (spaces, trailing) = match stars.cmp(&real_stars) {
                Ordering::Greater => {
                    let trailing = stars - real_stars - 1;
                    stars = real_stars;
                    (0, trailing)
                }
                Ordering::Less => (real_stars - stars, 0),
                Ordering::Equal => (0, 0),
            };
            writeln_stdout!(
                "{:*<3$}{: <4$}|{:*<5$}",
                "",
                "",
                "",
                stars,
                spaces,
                trailing
            );
        }
    }
}

#[derive(Parser)]
#[clap(about = "Print out the current hits-by-size histogram.")]
#[clap(alias = "report_hits")]
pub struct Hits {
    /// Divide hit data into this many buckets.
    #[clap(short = 'q', long, default_value = "20", conflicts_with = "clear")]
    quantiles: usize,

    /// Use JSON output format.
    #[clap(
        short = 'j',
        long,
        conflicts_with = "clear",
        conflicts_with = "quantiles"
    )]
    json: bool,

    /// Clear the current hit-by-size histogram
    #[clap(short = 'c', long)]
    clear: bool,
}

#[async_trait]
impl ZcacheSubCommand for Hits {
    async fn invoke(&self) -> Result<()> {
        let mut remote = RemoteChannel::new(self.clear).await?;

        // a request to clear hit data
        if self.clear {
            match remote.call(TYPE_CLEAR_HIT_DATA, None).await {
                Ok(_) => {
                    writeln_stdout!("Hits-by-size data cleared");
                }
                Err(RemoteError::ResultError(_)) => {
                    return Err(anyhow!(
                        "No cache found, so no hits-by-size data is available"
                    ))
                }
                Err(RemoteError::Other(e)) => return Err(e),
            }
            return Ok(());
        }

        match remote.call(TYPE_REPORT_HITS, None).await {
            Ok(response) => {
                let response: ReportHitsResponse = nvpair::from_nvlist(&response)?;
                let json_quantiles = response.cache_capacity / response.bucket_size;
                let hits_by_size = HitsBySize::new(
                    response,
                    if self.json {
                        // For JSON keep all the data points (don't down sample)
                        usize::from64(json_quantiles)
                    } else {
                        self.quantiles
                    },
                );

                if self.json {
                    writeln_stdout!("{}", serde_json::to_string_pretty(&hits_by_size).unwrap());
                } else {
                    hits_by_size.print();
                }
            }
            Err(RemoteError::ResultError(_)) => {
                return Err(anyhow!(
                    "No cache found, so no hits-by-size data is available"
                ));
            }
            Err(RemoteError::Other(e)) => return Err(e),
        }
        Ok(())
    }
}

/// Legacy CLI for clearing hit data (deprecated and hidden)
#[derive(Parser, Debug)]
#[clap(about = "Clear the current hit-by-size histogram")]
#[clap(hide = true)]
// XXX can't seem to apply the 'snake_case' here so it is applied in enum Commands
pub struct ClearHitData;

#[async_trait]
impl ZcacheSubCommand for ClearHitData {
    async fn invoke(&self) -> Result<()> {
        let hits = Hits {
            quantiles: 0,
            json: false,
            clear: true,
        };
        hits.invoke().await
    }
}
