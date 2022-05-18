//! `zcache hits` subcommand

use std::cmp::max;
use std::cmp::Ordering;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use clap::Parser;
use num_traits::cast::ToPrimitive;
use serde::Serialize;
use util::message::TYPE_CLEAR_HIT_DATA;
use util::message::TYPE_REPORT_HITS;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
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
        let report_hits = report_hits.resampled(quantiles);

        let mut accumulator = 0;
        HitsBySize {
            start_time: report_hits.started.into(),
            end_time: Utc::now(),
            cache_lookups: report_hits.lookups,
            cache_hits: report_hits.real_hits,
            cache_capacity: report_hits.cache_capacity,
            bucket_size: report_hits.bucket_size,
            hits_report: report_hits
                .combined_histogram
                .iter()
                .map(|value| {
                    accumulator += value;
                    accumulator
                })
                .collect(),
        }
    }

    fn print(&self, requested_histogram_width: Option<usize>) {
        writeln_stdout!(
            "Data collection started: {}",
            self.start_time.with_timezone(&Local).to_rfc2822()
        );
        writeln_stdout!(
            "Data collection ended:   {} ({})",
            self.end_time.with_timezone(&Local).to_rfc2822(),
            humantime::format_duration(Duration::from_secs(
                (self.end_time - self.start_time)
                    .to_std()
                    .unwrap()
                    .as_secs()
            )),
        );
        let real_hits_percent = if self.cache_lookups == 0 {
            100.0
        } else {
            self.cache_hits as f64 * 100.0 / self.cache_lookups as f64
        };
        writeln_stdout!(
            "Cache Hits: {real_hits_percent:.1}% in {} cache ({} lookups with {} hits)",
            nice_p2size(self.cache_capacity),
            self.cache_lookups,
            self.cache_hits,
        );

        if self.hits_report.is_empty() {
            return;
        }

        const PREFIX_WIDTH: usize = 18;
        const MIN_HISTOGRAM_WIDTH: usize = 25;
        let pentile_width = match requested_histogram_width {
            Some(value) => max(MIN_HISTOGRAM_WIDTH, value) / 5,
            None => {
                let terminal_width = max(
                    PREFIX_WIDTH + MIN_HISTOGRAM_WIDTH,
                    match termsize::get() {
                        None => 80,
                        Some(size) => size.cols as usize,
                    },
                );
                (terminal_width - PREFIX_WIDTH) / 5
            }
        };
        let histogram_width = pentile_width * 5;

        writeln_stdout!(
            "\n{:>7$} {:>1}{:>8$}{:>8$}{:>8$}{:>8$}{:>9$}",
            "size :   hit%",
            0,
            20,
            40,
            60,
            80,
            100,
            PREFIX_WIDTH - 1,
            pentile_width,
            pentile_width - 1,
        );
        writeln_stdout!("{0:-<1$}", "-", histogram_width + PREFIX_WIDTH);

        for (index, &cumulative_hits) in self.hits_report.iter().enumerate() {
            let cache_size_at_bucket = (index + 1) as u64 * self.bucket_size;
            let percent = (cumulative_hits * 100) as f64 / self.cache_lookups as f64;
            if percent >= 99.95 && cache_size_at_bucket > self.cache_capacity {
                break;
            }
            write_stdout!(
                "{: >8} : {percent: >5.1}% ",
                nice_p2size(cache_size_at_bucket)
            );
            if self.cache_hits == 0 {
                writeln_stdout!();
                continue;
            }

            let total_stars = (histogram_width as f64 * percent / 100.0)
                .ceil()
                .to_usize()
                .unwrap();
            let bar_position = (histogram_width as f64 * real_hits_percent / 100.0)
                .ceil()
                .to_usize()
                .unwrap();
            let (stars_before_bar, spaces_before_bar, stars_after_bar) = match total_stars
                .cmp(&bar_position)
            {
                Ordering::Less | Ordering::Equal => (total_stars, bar_position - total_stars, 0),
                Ordering::Greater => (bar_position, 0, total_stars - bar_position - 1),
            };
            writeln_stdout!(
                "{:*<3$}{: <4$}|{:*<5$}",
                "",
                "",
                "",
                stars_before_bar,
                spaces_before_bar,
                stars_after_bar
            );
        }
    }
}

#[derive(Parser)]
#[clap(about = "Print out the current hits-by-size histogram.")]
#[clap(alias = "report_hits")]
pub struct Hits {
    /// Divide hit data into this many buckets (default: based on terminal height, or fit to 24
    /// rows)
    #[clap(short = 'q', long, conflicts_with = "clear")]
    quantiles: Option<usize>,

    /// Display histogram with this many columns (default: based on terminal width, or fit to 80
    /// columns)
    #[clap(short = 'w', long, conflicts_with = "clear")]
    width: Option<usize>,

    /// Use JSON output format.
    #[clap(
        short = 'j',
        long,
        conflicts_with = "clear",
        conflicts_with = "quantiles",
        conflicts_with = "width"
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
                let quantiles = if self.json {
                    // For JSON keep all the data points (don't down sample)
                    response.combined_histogram.len()
                } else if let Some(quantiles) = self.quantiles {
                    quantiles
                } else {
                    const HEADER_ROWS: usize = 6;
                    const MIN_QUANTILES: usize = 5;
                    const BUFFER_ROWS: usize = 2; // for the previous and next command prompts
                    let terminal_height = max(
                        HEADER_ROWS + MIN_QUANTILES + BUFFER_ROWS,
                        match termsize::get() {
                            None => 24,
                            Some(size) => size.rows as usize,
                        },
                    );
                    terminal_height - HEADER_ROWS - BUFFER_ROWS
                };
                let hits_by_size = HitsBySize::new(response, quantiles);

                if self.json {
                    writeln_stdout!("{}", serde_json::to_string_pretty(&hits_by_size).unwrap());
                } else {
                    hits_by_size.print(self.width);
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
            quantiles: None,
            width: None,
            json: false,
            clear: true,
        };
        hits.invoke().await
    }
}
