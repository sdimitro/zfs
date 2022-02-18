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
use util::message::TYPE_REPORT_HITS;
use util::nice_p2size;
use util::From64;
use util::{write_stdout, writeln_stdout};

static NAME: &str = "report_hits";

struct SizeHistogram {
    start: SystemTime,
    lookups: u64,
    cache_capacity: u64,
    bucket_size: u64,
    live_histogram: Vec<u64>,
    ghost_histogram: Vec<u64>,
}

impl SizeHistogram {
    fn sum_live_hits(&self) -> u64 {
        self.live_histogram.iter().sum()
    }

    /// Resample a histogram to produce a new histogram with the requested
    /// number of buckets for the capacity portion of the original histogram.
    /// This works by dividing each sample in the original histogram into "samples"
    /// chunks and then adding the number of samples for the physical cache in the
    /// original histogram of these chunks together for each bucket in the new histogram.
    fn resample(&self, samples_in_capacity: usize, histogram: &[u64]) -> Vec<u64> {
        // Given the desired number of samples for the cache capacity portion of the histogram,
        // calculate the total number of samples needed for the capacity covered by the histogram.
        let resample_bucket_size = self.cache_capacity / samples_in_capacity as u64;
        let target_samples = (histogram.len() as f64 * self.bucket_size as f64
            / resample_bucket_size as f64)
            .ceil()
            .to_usize()
            .unwrap();

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
        assert_eq!(resample.len(), target_samples);
        resample
    }

    /// print out a histogram of hits-by-cache-size
    fn print(&self, quantiles: usize, cumulative: bool, ghost: bool) {
        let start_as_utc: DateTime<Local> = self.start.into();
        writeln_stdout!("Data collection started: {}", start_as_utc.to_rfc2822());
        writeln_stdout!("Data collection ended: {}", Local::now().to_rfc2822());
        let total = self.sum_live_hits();
        write_stdout!(
            "Cache Hits by Size ({} lookups with {} hits ",
            self.lookups,
            total
        );
        let hit_percent = if self.lookups == 0 {
            100.0
        } else {
            total as f64 * 100.0 / self.lookups as f64
        };
        writeln_stdout!(
            "({:.1}%) in {} cache)",
            hit_percent,
            nice_p2size(self.cache_capacity)
        );
        if quantiles == 0 {
            return;
        }
        const HISTOGRAM_WIDTH: usize = 50;
        let histogram_length = self.live_histogram.len() as u64;
        let histogram_capacity = histogram_length * self.bucket_size;
        let mut bucket_total = 0;
        let mut cache_size = 0;
        let live_histogram = self.resample(quantiles, &self.live_histogram);
        let ghost_histogram = self.resample(quantiles, &self.ghost_histogram);
        let bucket_size = self.bucket_size * histogram_length / live_histogram.len() as u64;
        let mut beyond_live = false;

        for (index, (live_hits, ghost_hits)) in
            live_histogram.into_iter().zip(ghost_histogram).enumerate()
        {
            if !beyond_live && (ghost_hits > live_hits || index == quantiles) {
                if !ghost {
                    return;
                }
                writeln_stdout!("-------------------ghost hits---------------------");
                beyond_live = true;
            }
            cache_size += bucket_size;
            // The last bucket may not be the "full" bucket size
            if cache_size > histogram_capacity {
                assert!(
                    index == self.live_histogram.len() - 1,
                    "Capacity overflow at histogram index {}",
                    index
                );
                cache_size = histogram_capacity;
            }

            write_stdout!("{: >8} : ", nice_p2size(cache_size));
            if total == 0 {
                writeln_stdout!();
                continue;
            }
            if cumulative {
                bucket_total += live_hits + ghost_hits;
            } else {
                bucket_total = live_hits + ghost_hits;
            };
            if bucket_total == 0 {
                // this bucket is empty (if we are accumulating, no hits have been seen yet)
                writeln_stdout!("  0%");
                continue;
            }
            let percent = (bucket_total as f64 * hit_percent) / total as f64;
            if percent < 1.0 {
                // there are a small number of hits
                writeln_stdout!(" <1% *");
            } else {
                let stars = std::cmp::max(percent.to_usize().unwrap() * HISTOGRAM_WIDTH / 100, 1);
                writeln_stdout!("{: >3.0}% {:*<2$}", percent, "", stars);
            }
        }
    }
}

pub struct ReportHits;

#[async_trait]
impl ZcacheSubCommand for ReportHits {
    fn subcommand(&self) -> clap::App<'static, 'static> {
        fn valid_int(v: String) -> Result<(), String> {
            match v.parse::<usize>() {
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }

        SubCommand::with_name(NAME)
            .about("Print out the current hit-by-size histogram.")
            .arg(
                Arg::with_name("quantiles")
                    .long("quantiles")
                    .short("q")
                    .default_value("20")
                    .validator(valid_int)
                    .help("Divide report data into this many buckets"),
            )
            .arg(
                Arg::with_name("non-cumulative")
                    .long("non-cumulative")
                    .short("n")
                    .help("Don't accumulate hits from previous quantiles"),
            )
            .arg(
                Arg::with_name("only-live-hits")
                    .long("only-live-hits")
                    .short("o")
                    .help("Don't show ghost hit data"),
            )
    }

    fn name(&self) -> String {
        NAME.to_string()
    }

    async fn invoke(&mut self, args: &clap::ArgMatches) -> Result<()> {
        let quantiles = args.value_of("quantiles").unwrap().parse()?;
        let cumulative = !args.is_present("non-cumulative");
        let ghost = !args.is_present("only-live-hits");

        let mut remote = RemoteChannel::new(false).await?;
        match remote.call(TYPE_REPORT_HITS, None).await {
            Ok(response) => {
                let hits_by_size = SizeHistogram {
                    start: UNIX_EPOCH + Duration::new(response.lookup_uint64("started")?, 0),
                    lookups: response.lookup_uint64("lookups")?,
                    cache_capacity: response.lookup_uint64("cache_capacity")?,
                    bucket_size: response.lookup_uint64("bucket_size")?,
                    live_histogram: response.lookup_uint64_array("live_histogram")?,
                    ghost_histogram: response.lookup_uint64_array("ghost_histogram")?,
                };
                hits_by_size.print(quantiles, cumulative, ghost);
            }
            Err(RemoteError::ResultError(_)) => {
                writeln_stdout!("No cache found, so no hits-by-size data is available");
            }
            Err(RemoteError::Other(e)) => return Err(e),
        }
        Ok(())
    }
}
