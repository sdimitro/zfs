use crate::remote_channel::RemoteChannel;
use crate::remote_channel::RemoteError;
use crate::subcommand::ZcacheSubCommand;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Local};
use clap::Arg;
use clap::SubCommand;
use num_traits::cast::ToPrimitive;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use util::nice_p2size;
use util::From64;

static NAME: &str = "report_hits";

struct SizeHistogram {
    start: SystemTime,
    lookups: u64,
    bucket_size: u64,
    histogram: Vec<u64>,
}

impl SizeHistogram {
    fn sum(&self) -> u64 {
        self.histogram.iter().sum()
    }

    /// Resample the histogram to produce a new histogram
    /// with the requested number of buckets. This works by dividing
    /// each sample in the original histogram into "samples" chunks and
    /// then adding histogram.len() of these chunks together for each bucket
    /// in the new histogram.
    fn resample(&self, samples: usize) -> Vec<u64> {
        let mut sample_iter = self.histogram.iter();
        let mut sub_sample_value = 0.0;
        let mut samples_left = 0;
        let mut resample: Vec<u64> = Vec::new();
        'outer: loop {
            let mut accumulated_value = 0.0;
            let mut needed_samples = self.histogram.len();
            while needed_samples > 0 {
                if samples_left == 0 {
                    match sample_iter.next() {
                        Some(sample) => {
                            sub_sample_value = *sample as f64 / samples as f64;
                        }
                        None => break 'outer,
                    }
                    samples_left = samples;
                }
                let samples_to_add = std::cmp::min(needed_samples, samples_left);
                accumulated_value += samples_to_add as f64 * sub_sample_value;
                needed_samples -= samples_to_add;
                samples_left -= samples_to_add;
            }
            resample.push(accumulated_value.round().to_u64().unwrap());
        }
        assert_eq!(resample.len(), samples);
        resample
    }

    fn print(&self, quantiles: usize, cumulative: bool) {
        assert_eq!(self.histogram.len(), 100);
        let total = self.sum();
        // print out a histogram
        let start_as_utc: DateTime<Local> = self.start.into();
        println!("Data collection started: {}", start_as_utc.to_rfc2822());
        println!("Data collection ended: {}", Local::now().to_rfc2822());
        print!(
            "Cache Hits by Size ({} lookups with {} hits ",
            self.lookups, total
        );
        let hit_percent = if self.lookups == 0 {
            100.0
        } else {
            total as f64 * 100.0 / self.lookups as f64
        };
        println!(
            "({:.1}%) in {} cache)",
            hit_percent,
            nice_p2size(self.bucket_size * self.histogram.len() as u64)
        );
        if quantiles == 0 {
            return;
        }
        const HISTOGRAM_WIDTH: usize = 50;
        let mut subtotal = 0;
        let mut cache_size = 0;
        let resampled_histogram = self.resample(quantiles);
        let bucket_size: u64 = self.bucket_size * self.histogram.len() as u64 / quantiles as u64;
        for count in &resampled_histogram {
            cache_size += bucket_size;
            subtotal += count;
            print!("{: >8} : ", nice_p2size(cache_size));
            if total == 0 {
                println!();
                continue;
            }
            let percent = if cumulative {
                (subtotal * hit_percent.to_u64().unwrap()) / total
            } else {
                (count * hit_percent.to_u64().unwrap()) / total
            };
            if percent == 0 && (subtotal == 0 || (!cumulative && *count == 0)) {
                // this bucket is empty (and, if we are accumulating, no hits have been seen yet)
                println!("  0%");
            } else if percent == 0 {
                // there are a small number of hits
                println!(" <1% *");
            } else {
                let stars = usize::from64(percent) * HISTOGRAM_WIDTH / 100;
                println!("{: >3}% {:*<2$}", percent, "", stars);
            }
        }
    }
}

pub struct ReportHits;

#[async_trait]
impl ZcacheSubCommand for ReportHits {
    fn subcommand(&self) -> clap::App<'static, 'static> {
        SubCommand::with_name(NAME)
            .about("Print out the current hit-by-size histogram.")
            .arg(
                Arg::with_name("quantiles")
                    .long("quantiles")
                    .short("q")
                    .default_value("20")
                    .help("Divide report data into this many buckets"),
            )
            .arg(
                Arg::with_name("non_cumulative")
                    .long("non_cumulative")
                    .short("n")
                    .help("Don't accumulate hits from previous quantiles"),
            )
    }

    fn name(&self) -> String {
        NAME.to_string()
    }

    async fn invoke(&mut self, args: &clap::ArgMatches) -> Result<()> {
        let quantiles = args.value_of("quantiles").unwrap().parse()?;
        let cumulative = !args.is_present("non_cumulative");

        let mut remote = RemoteChannel::new(false).await?;
        match remote.call(NAME, None).await {
            Ok(response) => {
                let hits_by_size = SizeHistogram {
                    start: UNIX_EPOCH + Duration::new(response.lookup_uint64("started")?, 0),
                    lookups: response.lookup_uint64("lookups")?,
                    bucket_size: response.lookup_uint64("bucket_size")?,
                    histogram: response.lookup_uint64_array("histogram")?,
                };
                hits_by_size.print(quantiles, cumulative);
            }
            Err(RemoteError::ResultError(_)) => {
                println!("No cache found, so no hits-by-size data is available");
            }
            Err(RemoteError::Other(e)) => return Err(e),
        }
        Ok(())
    }
}
