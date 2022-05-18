//! The iostat subcommand for zcache.

use std::cmp::max;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use clap::Parser;
use log::*;
use util::flush_stdout;
use util::message::TYPE_ZCACHE_IOSTAT;
use util::nice_number_time;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
use util::zettacache_stats::*;

use crate::remote_channel::RemoteChannel;
use crate::subcommand::ZcacheSubCommand;

struct IoStatDisplay {
    show_active: bool,
    show_devices: bool,
    show_time: bool,
    interval_is_subsecond: bool,
    interval: Option<Duration>,
    count: Option<u64>,
    max_name_length: usize, // used for the device name column
    histogram_name: Option<String>,
}

impl IoStatDisplay {
    const TIME_WIDTH: usize = 10;
    const VALUE_WIDTH: usize = 6;
    const HEADER_HEIGHT: usize = 3;

    fn time_width(&self) -> usize {
        // When the time interval is under a second display sub-second time
        if self.interval_is_subsecond {
            // we add 4 chars (e.g. ".756") but there were 2 free chars (so net of +2)
            IoStatDisplay::TIME_WIDTH + 2
        } else {
            IoStatDisplay::TIME_WIDTH
        }
    }

    fn display_dashes(width: usize) {
        write_stdout!("{0:-<1$}  ", "-", width);
    }

    /// Display centered column group titles, such as 'Lookup: Read-Data'
    fn display_header(&self, titles: Vec<&str>, columns_per_group: usize) {
        if self.show_time {
            write_stdout!("{:^1$}  ", "timestamp", self.time_width());
        }
        if self.show_devices {
            write_stdout!("{:^1$}  ", "zcache", self.max_name_length);
        }
        let column_group_width = (columns_per_group * (IoStatDisplay::VALUE_WIDTH + 2)) - 2;
        for title in titles {
            write_stdout!("{:^1$}    ", title, column_group_width);
        }
        writeln_stdout!();
    }

    /// Print a line comprised of a set of headers repeated `copies` times.
    /// Also prints another line with a dashed line below each column.
    fn display_column(&self, headers: Vec<&str>, copies: usize) {
        let last_group = copies - 1;

        // First print column headers
        if self.show_time {
            // Note: we use the date as the header over the timestamp column
            write_stdout!(
                "{0:>1$}  ",
                format!("{}", Local::now().format("%Y-%m-%d")),
                self.time_width()
            );
        }
        if self.show_devices {
            write_stdout!("{:^1$}  ", "device", self.max_name_length);
        }
        for cg in 0..copies {
            for h in &headers {
                let spacing: usize = if h.len() > IoStatDisplay::VALUE_WIDTH {
                    // Adjust spacing to accommodate column headers that are > 6, like 'latency'
                    h.len() - IoStatDisplay::VALUE_WIDTH
                } else {
                    2 // default is two spaces
                };
                write_stdout!("{0:^1$}{2:3$}", h, IoStatDisplay::VALUE_WIDTH, "", spacing);
            }
            if cg != last_group {
                write_stdout!("  "); // Separate column groups by two additional spaces
            }
        }
        writeln_stdout!();

        // Now print dashes underneath each column
        if self.show_time {
            IoStatDisplay::display_dashes(self.time_width());
        }
        if self.show_devices {
            IoStatDisplay::display_dashes(self.max_name_length);
        }
        for cg in 0..copies {
            for _h in 0..headers.len() {
                IoStatDisplay::display_dashes(IoStatDisplay::VALUE_WIDTH);
            }
            if cg != last_group {
                write_stdout!("  "); // Separate column groups by 2 additional spaces
            }
        }
        writeln_stdout!();
    }

    /// Display 3 rows worth of headers which includes a row of dashes below each column
    /// A typical column group looks like:
    /// ```
    ///   Lookup: Read-Data
    ///  iops   amount  latency
    /// ------  ------  ------
    /// ```
    fn display_all_headers(&self) {
        let titles = vec![
            "Lookup: Read-Data",
            "Lookup: Read-Index",
            "Insert: Write-Data",
            "Maintenance: Read",
            "Maintenance: Write",
        ];
        let column_groups = titles.len();

        let mut columns = vec!["iops", "amount", "latency"];
        if self.show_active {
            columns.push("active")
        }

        self.display_header(titles, columns.len());
        self.display_column(columns, column_groups);
    }

    fn iterations_per_header(&self, device_count: usize) -> u64 {
        let mut terminal_height = max(
            IoStatDisplay::HEADER_HEIGHT + device_count + 1,
            match termsize::get() {
                None => 24,
                Some(size) => size.rows as usize,
            },
        );

        terminal_height -= IoStatDisplay::HEADER_HEIGHT;
        if self.show_devices {
            // With devices there will be device_count + 1 rows instead of one
            terminal_height /= device_count + 1;
        }
        terminal_height as u64
    }

    fn display_one_row(&self, disk: &DiskIoStats, elapsed: Duration) {
        // Some values are scaled to match the interval time
        let scale = if elapsed.as_nanos() == 0 {
            1.0
        } else {
            1_000_000_000.0 / elapsed.as_nanos() as f64
        };

        for stat_values in disk.stats.values() {
            stat_values.operations.display_pretty(Some(scale));
            stat_values.total_bytes.display_pretty(Some(scale));

            let nanoseconds = &stat_values.total_nanoseconds.0.load(Relaxed);
            let operations = &stat_values.operations.0.load(Relaxed);
            StatLatency(Duration::from_nanos(
                nanoseconds.checked_div(*operations).unwrap_or_default(),
            ))
            .display_pretty();

            // active count is only displayed if requested
            if self.show_active {
                stat_values.active_count.display_pretty(None);
            }
            write_stdout!("  "); // Note we pad with two additional spaces between groups
        }
        writeln_stdout!();
    }

    /// Display the default iostat output.
    fn display_iostat_default(&self, iteration: u64, stat_delta: &IoStats) {
        // Periodically display the column headers
        if (iteration % self.iterations_per_header(stat_delta.disk_stats.len())) == 0 {
            debug!(
                "collected stats interval {}",
                nice_number_time(stat_delta.timestamp)
            );
            self.display_all_headers();
        }

        for (i, disk_stats) in stat_delta.disk_stats.iter().enumerate() {
            if self.show_time {
                let time = if i == 0 {
                    if self.interval_is_subsecond {
                        // e.g. "05:43:54.254"
                        Local::now().format("%H:%M:%S%.3f").to_string()
                    } else {
                        // e.g. "05:43:54"
                        Local::now().format("%H:%M:%S").to_string()
                    }
                } else {
                    // Show the time once (above) in 'summary' row, but not with each device row
                    String::from("")
                };
                write_stdout!("{:>1$}  ", time, self.time_width());
            }
            if self.show_devices {
                let (width, indent) = if i == 0 {
                    (self.max_name_length, "")
                } else {
                    (self.max_name_length - 2, "  ")
                };
                write_stdout!("{}{:<2$}  ", indent, disk_stats.name, width);
            }

            self.display_one_row(disk_stats, stat_delta.timestamp);
            if !self.show_devices {
                break;
            }
        }
        if self.show_devices {
            writeln_stdout!()
        }
    }

    fn display_histogram_headers(&self, name: &str, headers: Vec<&str>) {
        write_stdout!("{:<1$} ", name, self.max_name_length);

        for column in headers {
            write_stdout!("{:^1$}", column, IoStatDisplay::VALUE_WIDTH + 2);
        }
        writeln_stdout!();
    }

    /// Print the histogram iostat output.
    fn display_iostat_histogram(&self, histogram_name: &str, stat_delta: &IoStats) {
        for disk_stat in stat_delta.disk_stats.iter() {
            if self.show_time {
                if self.interval_is_subsecond {
                    writeln_stdout!("{}", Local::now().format("%Y-%m-%d %H:%M:%S%.3f UTC"));
                } else {
                    writeln_stdout!("{}", Local::now().format("%Y-%m-%d %H:%M:%S UTC"));
                }
            }
            writeln_stdout!();
            let device_name = if self.show_devices {
                &disk_stat.name
            } else {
                ""
            };
            self.display_histogram_headers(
                device_name,
                vec!["lookup", "lookup", "insert", "maint", "maint"],
            );
            self.display_histogram_headers(
                histogram_name,
                vec!["data", "index", "data", "read", "write"],
            );
            self.display_histogram_headers(
                &format!("{:-<1$}", "-", self.max_name_length),
                vec!["------"; 5],
            );

            if histogram_name.eq("latency") {
                for j in 0..LatencyHistogram::BUCKETS {
                    // First display each bucket name
                    // Use ending range of bucket for latency (first bucket is 1us)
                    let nice_value = nice_number_time(Duration::from_nanos((1024 << j) - 1));
                    write_stdout!("{:>1$} ", nice_value, self.max_name_length);

                    // Then display bucket values for each disk io type (total of 5)
                    for v in disk_stat.stats.values() {
                        let count = &v.latency_histogram.0[j];
                        count.display_pretty(None);
                    }
                    writeln_stdout!();
                }
            } else {
                for j in 0..RequestHistogram::BUCKETS {
                    // First display each bucket name
                    // Use starting range of bucket for request sizes (first bucket is 512B)
                    let nice_value = nice_p2size(512 << j);
                    write_stdout!("{:>1$} ", nice_value, self.max_name_length);

                    // Then display bucket values for each disk io type (total of 5)
                    for v in disk_stat.stats.values() {
                        let count = &v.request_histogram.0[j];
                        count.display_pretty(None);
                    }
                    writeln_stdout!();
                }
            }

            // Print a line of dashes after each histogram
            writeln_stdout!(
                "{:-<1$}",
                "-",
                self.max_name_length + ((IoStatDisplay::VALUE_WIDTH + 2) * disk_stat.stats.len())
            );
            if !self.show_devices {
                break;
            }
        }
    }

    async fn display_io_stats(&mut self) -> Result<()> {
        let mut iteration = 0;
        let mut previous = IoStats::default(); // place holder empty stats
        let mut remote = RemoteChannel::new(false).await?;

        loop {
            let response = remote.call(TYPE_ZCACHE_IOSTAT, None).await?;
            let io_stats_json = response.lookup_string("iostats_json")?;
            let mut latest: IoStats = serde_json::from_str(io_stats_json.to_str()?)?;

            if self.show_devices {
                // +2 on device names to account for indenting devices under 'summary'
                self.max_name_length = max(self.max_name_length, latest.max_name_len() + 2);
                self.max_name_length = max(self.max_name_length, "summary".len());
            }

            // Create a summary disk stat of all the devices
            IoStatDisplay::insert_summary_disk(&mut latest);

            debug!("{latest:?}");

            if previous.disk_stats.is_empty()
                || latest.cache_runtime_id == previous.cache_runtime_id
            {
                let delta = &latest - &previous;

                match &self.histogram_name {
                    None => self.display_iostat_default(iteration, &delta),
                    Some(name) => self.display_iostat_histogram(name, &delta),
                }
                // Flush stdout in case output is redirected to a file
                flush_stdout()?;
            } else {
                info!("object agent restarted");
            }

            iteration += 1;
            let interval: Duration = match self.interval {
                None => return Ok(()),
                Some(interval) => interval,
            };

            if self.interval.is_none() {
                return Ok(());
            }
            if let Some(count) = self.count {
                if iteration >= count {
                    return Ok(());
                }
            }

            previous = latest;
            sleep(interval);
        }
    }

    fn insert_summary_disk(disk_stats: &mut IoStats) {
        let mut summary = DiskIoStats::new("summary".to_string());

        for disk in &disk_stats.disk_stats {
            summary += disk;
        }
        disk_stats.disk_stats.insert(0, summary);
    }
}

#[derive(Parser)]
#[clap(about = "Display I/O statistics.")]
pub struct Iostat {
    /// Include active queue statistics
    #[clap(
        short = 'a',
        long,
        conflicts_with = "latency-histogram",
        conflicts_with = "request-size-histogram"
    )]
    active: bool,

    /// Reports the statistics for individual devices in the zettacache
    #[clap(short = 'd', long)]
    devices: bool,

    /// Display latency histograms
    #[clap(short = 'l', long, conflicts_with = "request-size-histogram")]
    latency_histogram: bool,

    /// Display request size histograms for each I/O type
    #[clap(short = 'r', long, conflicts_with = "latency-histogram")]
    request_size_histogram: bool,

    /// Display a timestamp on each line of iostats
    #[clap(short = 't', long)]
    timestamp: bool,

    /// Statistics are printed every <interval> seconds
    #[clap()]
    interval: Option<f64>,

    /// Stop after <count> reports have been displayed
    #[clap()]
    count: Option<u64>,
}

#[async_trait]
impl ZcacheSubCommand for Iostat {
    async fn invoke(&self) -> Result<()> {
        let interval = self.interval.map(Duration::from_secs_f64);
        let histogram_name = if self.latency_histogram {
            Some("latency".to_string())
        } else if self.request_size_histogram {
            Some("req-size".to_string())
        } else {
            None
        };

        let max_name_length = histogram_name
            .as_ref()
            .map(|name| name.len())
            .unwrap_or_default();

        IoStatDisplay {
            show_time: self.timestamp,
            show_active: self.active,
            show_devices: self.devices,
            max_name_length,
            histogram_name,
            interval,
            count: self.count,
            interval_is_subsecond: interval.map_or(false, |d| d.as_secs() < 1),
        }
        .display_io_stats()
        .await
    }
}
