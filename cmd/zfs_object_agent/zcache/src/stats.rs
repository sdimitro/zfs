//! zcache stats subcommand

use crate::remote_channel::{RemoteChannel, RemoteError};
use crate::subcommand::ZcacheSubCommand;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use clap::{Arg, SubCommand};
use log::*;
use num_traits::cast::ToPrimitive;
use std::cmp::max;
use std::io::{self, Write};
use std::thread::sleep;
use std::time::Duration;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::*;
use util::{nice_number_count, nice_p2size};

static NAME: &str = "stats";
static REQUEST: &str = "zcache_stats";

struct StatsDisplay {
    show_time: bool,
    show_extended: bool,
    show_insert_detail: bool,
    show_lookup_detail: bool,
    show_block_allocator: bool,
    show_exact_values: bool,
    interval_is_subsecond: bool,
    interval: Option<Duration>,
    count: Option<u64>,
}

impl StatsDisplay {
    const VALUE_WIDTH: usize = 6; // widest nice number, e.g. "9.20MB"
    const TIME_WIDTH: usize = 10; // a date header, e.g. "2021-12-19"

    fn time_width(&self) -> usize {
        // When the time interval is under a second display sub-second time
        if self.interval_is_subsecond {
            // we add 4 chars (e.g. ".756") but there were 2 free chars (so net of +2)
            StatsDisplay::TIME_WIDTH + 2
        } else {
            StatsDisplay::TIME_WIDTH
        }
    }

    fn display_bytes(&self, bytes: f64) {
        let value: u64 = bytes.round().to_u64().unwrap();
        if self.show_exact_values {
            print!("{:0}\t", value);
        } else if value == 0 {
            // Intentionally avoid displaying "0.00B" when 0
            print!("{:>6}  ", "0");
        } else {
            print!("{:>6}  ", nice_p2size(value));
        }
    }

    fn display_count(&self, count: f64) {
        if self.show_exact_values {
            let value: u64 = count.round().to_u64().unwrap();
            print!("{:0}\t", value);
        } else if count == 0.0 {
            // Intentionally avoid displaying "0.00" when 0
            print!("{:>6}  ", "0");
        } else {
            print!("{:>6}  ", nice_number_count(count));
        }
    }

    fn display_percent(&self, value: f64, total: f64) {
        let percent = if total == 0.0 {
            0.0
        } else {
            (value * 100.0) / total
        };

        if self.show_exact_values {
            print!("{:0.0}\t", percent.round());
        } else if percent < 0.5 {
            print!("{:>6}  ", "-");
        } else {
            print!("{:>5.0}%  ", percent.round());
        }
    }

    fn display_timestamp(&self) {
        if self.show_exact_values {
            // e.g. "1639893294"
            print!("{}", Local::now().format("%s%t"));
        } else {
            let time = if self.interval_is_subsecond {
                // e.g. "05:43:54.254"
                Local::now().format("%H:%M:%S%.3f")
            } else {
                // e.g. "05:43:54"
                Local::now().format("%H:%M:%S")
            };
            print!("{0:>1$}  ", time, self.time_width());
        }
    }

    fn display_dashes(width: usize) {
        print!("{0:-<1$}  ", "-", width);
    }

    fn display_headers_impl(&self, top: Vec<(&str, usize)>, bottom: Vec<&str>) {
        if self.show_time {
            print!("{0:^1$}  ", "TIMESTAMP", self.time_width());
        }
        for (header, n) in top.iter() {
            let width = (n * (StatsDisplay::VALUE_WIDTH + 2)) - 2;
            print!("{0:^1$}  ", header, width);
        }
        println!();

        if self.show_time {
            print!(
                "{0:>1$}  ",
                format!("{}", Local::now().format("%Y-%m-%d")),
                self.time_width()
            );
        }
        for h in bottom.iter() {
            let spacing = if h.len() > StatsDisplay::VALUE_WIDTH {
                // Adjust spacing to accommodate column headers that are > 6 characters
                h.len() - StatsDisplay::VALUE_WIDTH
            } else {
                2 // default is two spaces
            };
            print!("{0:^1$}{2:3$}", h, StatsDisplay::VALUE_WIDTH, "", spacing);
        }
        println!();

        // Now print dashes underneath each column
        if self.show_time {
            StatsDisplay::display_dashes(self.time_width());
        }

        for _h in 0..bottom.len() {
            StatsDisplay::display_dashes(StatsDisplay::VALUE_WIDTH);
        }
        println!();
    }

    fn display_headers(&self) {
        // Produces header output like below. There can be additional opt-in headers.
        //
        // TIMESTAMP    CACHE-LOOKUP     CACHE-HIT       CACHE-MISS     CACHE-INSERT
        // 2022-01-12  count   bytes   count   ratio   count   ratio   count   bytes
        // ----------  ------  ------  ------  ------  ------  ------  ------  ------

        // Top headers is a vector of tuples: (header-title, column-count)
        let mut top_header: Vec<(&str, usize)> = vec![
            ("CACHE-LOOKUP", 2),
            ("CACHE-HIT", 2),
            ("CACHE-MISS", 2),
            ("CACHE-INSERT", 2),
        ];
        // Bottom headers is a vector of: header-column-name
        let mut bottom_header = vec![
            "count", "bytes", "count", "ratio", "count", "ratio", "count", "bytes",
        ];

        if self.show_lookup_detail {
            // Slot in right after "CACHE-LOOKUP" column
            top_header.insert(1, ("LOOKUP-SOURCE", 2));
            top_header.insert(2, ("HIT-READ-INDEX", 2));
            bottom_header.insert(2, "read");
            bottom_header.insert(3, "write");
            bottom_header.insert(4, "sans");
            bottom_header.insert(5, "after");
        }

        // the following optional headers are appended in the order presented here

        if self.show_insert_detail {
            top_header.append(&mut vec![("INSERT-SOURCE", 3), ("INSERT-DROPS", 2)]);
            bottom_header.append(&mut vec!["read", "write", "spec-r", "full-q", "lkbusy"]);
        }

        if self.show_extended {
            top_header.append(&mut vec![("BUF-BYTES-USED", 2)]);
            bottom_header.append(&mut vec!["block", "non-blk"]);
            top_header.append(&mut vec![("CACHE-OTHER", 3)]);
            bottom_header.append(&mut vec!["evicts", "pending", "healed"]);
        }

        if self.show_block_allocator {
            // Append after all other columns
            top_header.append(&mut vec![("BLOCK-ALLOCATOR", 3)]);
            bottom_header.append(&mut vec!["alloc", "avail", "cap"]);
        }

        self.display_headers_impl(top_header, bottom_header);
    }

    fn display_stat_values(&self, values: &CacheStats) {
        // Most values are scaled to account for the interval time
        let scale = if values.timestamp.as_nanos() == 0 {
            1.0
        } else {
            1.0 / values.timestamp.as_secs_f64()
        };
        debug!("interval {:?} has scaling {}", self.interval, scale);

        // TIMESTAMP (optional)
        if self.show_time {
            self.display_timestamp();
        }

        // LOOKUPS
        let total_lookups =
            (values.value(LookupForRead) + values.value(LookupForWrite)) as f64 * scale;
        self.display_count(total_lookups);
        self.display_bytes(values.value(LookupBytes) as f64 * scale);

        // LOOKUP DETAILS (optional)
        if self.show_lookup_detail {
            self.display_percent(values.value(LookupForRead) as f64 * scale, total_lookups);
            self.display_percent(values.value(LookupForWrite) as f64 * scale, total_lookups);

            let total_hits = (values.value(CacheHitWithoutIndexRead)
                + values.value(CacheHitAfterIndexRead)) as f64
                * scale;
            self.display_percent(
                values.value(CacheHitWithoutIndexRead) as f64 * scale,
                total_hits,
            );
            self.display_percent(
                values.value(CacheHitAfterIndexRead) as f64 * scale,
                total_hits,
            );
        }

        // HITS & MISSES
        let hits = (values.value(CacheHitWithoutIndexRead) + values.value(CacheHitAfterIndexRead))
            as f64
            * scale;
        let misses = (values.value(CacheMissAfterIndexRead)
            + values.value(CacheMissForcedEviction)
            + values.value(CacheMissWithoutIndexRead)) as f64
            * scale;
        self.display_count(hits);
        self.display_percent(hits, hits + misses);
        self.display_count(misses);
        self.display_percent(misses, hits + misses);

        // INSERTS
        let inserts = (values.value(InsertForRead)
            + values.value(InsertForWrite)
            + values.value(InsertForSpecRead)
            + values.value(InsertForHealing)) as f64
            * scale;
        self.display_count(inserts);
        self.display_bytes(values.value(InsertBytes) as f64 * scale);

        // INSERT DETAILS (optional)
        if self.show_insert_detail {
            self.display_percent(values.value(InsertForRead) as f64 * scale, inserts);
            self.display_percent(values.value(InsertForWrite) as f64 * scale, inserts);
            self.display_percent(values.value(InsertForSpecRead) as f64 * scale, inserts);
            self.display_count(values.value(InsertDropQueueFull) as f64 * scale);
            self.display_count(values.value(InsertDropLockBusy) as f64 * scale);
        }

        // EXTENDED (optional)
        if self.show_extended {
            self.display_bytes(values.value(BlockingBufferBytesAvailable) as f64);
            self.display_bytes(values.value(NonblockingBufferBytesAvailable) as f64);
            self.display_count(values.value(Evictions) as f64 * scale);
            // Note - PendingChanges stat is instantaneous so no need to scale
            self.display_count(values.value(PendingChanges) as f64);
            self.display_count(values.value(HealedBlocks) as f64 * scale);
        }

        // BLOCK-ALLOCATOR (optional)
        if self.show_block_allocator {
            let block_allocator_size = values.value(BlockAllocatorSize);
            let block_allocator_free = values.value(BlockAllocatorAvailable);
            let block_allocator_allocated = block_allocator_size - block_allocator_free;

            self.display_bytes(block_allocator_allocated as f64);
            self.display_bytes(block_allocator_free as f64);
            self.display_percent(
                block_allocator_allocated as f64,
                block_allocator_size as f64,
            );
        }

        println!();
    }

    async fn display_stats(&self) -> Result<()> {
        let mut iteration = 0;
        let mut previous = CacheStats::default(); // empty stats
        let mut remote = RemoteChannel::new(false).await?;

        loop {
            let latest: CacheStats;

            match remote.call(REQUEST, None).await {
                Ok(response) => {
                    let stats_json = response.lookup_string("stats_json").unwrap();
                    latest = serde_json::from_str(stats_json.to_str()?).unwrap();
                }
                Err(RemoteError::ResultError(_)) => {
                    println!("No cache found?");
                    continue;
                }
                Err(RemoteError::Other(e)) => {
                    println!("remote call error: {}", e);
                    // typically something like "Connection reset by peer (os error 104)"
                    return Err(e);
                }
            }

            // Periodically display the column headers
            if (iteration % (self.get_terminal_height() - 3) as u64) == 0 {
                self.display_headers();
            }

            // Display the net values of collected stats
            self.display_stat_values(&(&latest - &&previous));

            // Flush stdout in case output is redirected to a file
            io::stdout().flush().unwrap_or(());

            if let Some(count) = self.count {
                if iteration >= count {
                    return Ok(());
                }
            }

            match self.interval {
                None => return Ok(()),
                Some(duration) => {
                    previous = latest;
                    iteration += 1;
                    sleep(duration)
                }
            }
        }
    }

    fn get_terminal_height(&self) -> usize {
        max(
            24,
            termsize::get()
                .map(|size| size.rows as usize)
                .unwrap_or_default(),
        )
    }
}

pub struct Stats;

#[async_trait]
impl ZcacheSubCommand for Stats {
    fn subcommand(&self) -> clap::App<'static, 'static> {
        fn valid_interval(value: String) -> Result<(), String> {
            match value.parse::<f64>() {
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }

        fn valid_count(value: String) -> Result<(), String> {
            match value.parse::<u64>() {
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }

        SubCommand::with_name(NAME)
            .about("Display cache statistics.")
            .arg(
                Arg::with_name("timestamp")
                    .long("timestamp")
                    .short("t")
                    .help("Display a timestamp on each line of stats")
                    .conflicts_with("all"),
            )
            .arg(
                Arg::with_name("insert-detail")
                    .long("insert-detail")
                    .short("i")
                    .help("Display additional cache insert details")
                    .conflicts_with("all"),
            )
            .arg(
                Arg::with_name("lookup-detail")
                    .long("lookup-detail")
                    .short("l")
                    .help("Display additional cache lookup details")
                    .conflicts_with("all"),
            )
            .arg(
                Arg::with_name("block-allocator")
                    .long("block-allocator")
                    .short("b")
                    .help("Display additional block allocator details")
                    .conflicts_with("all"),
            )
            .arg(
                Arg::with_name("extended")
                    .long("extended")
                    .short("x")
                    .help("Display extended statistics")
                    .conflicts_with("all"),
            )
            .arg(
                Arg::with_name("parsable")
                    .long("parsable")
                    .short("p")
                    .help("Display numbers in parsable (exact) values"),
            )
            .arg(
                Arg::with_name("all")
                    .long("all")
                    .short("a")
                    .help("Display all possible columns (alias to -biltx)"),
            )
            .arg(
                Arg::with_name("interval")
                    .validator(valid_interval)
                    .help("Statistics are printed every <interval> seconds"),
            )
            .arg(
                Arg::with_name("count")
                    .validator(valid_count)
                    .help("Stop after <count> reports have been displayed"),
            )
    }

    fn name(&self) -> String {
        NAME.to_string()
    }

    async fn invoke(&mut self, args: &clap::ArgMatches) -> Result<()> {
        let interval = args
            .value_of("interval")
            .map(|interval| Duration::from_secs_f64(interval.parse().unwrap_or(0.0)));
        let count = args
            .value_of("count")
            .map(|count| count.parse().unwrap_or(0));
        let all = args.is_present("all");

        StatsDisplay {
            show_time: all || args.is_present("timestamp"),
            show_extended: all || args.is_present("extended"),
            show_insert_detail: all || args.is_present("insert-detail"),
            show_lookup_detail: all || args.is_present("lookup-detail"),
            show_block_allocator: all || args.is_present("block-allocator"),
            show_exact_values: args.is_present("parsable"),
            interval_is_subsecond: interval.map_or(false, |d| d.as_secs() < 1),
            interval,
            count,
        }
        .display_stats()
        .await
    }
}
