//! zcache stats subcommand

use crate::remote_channel::{RemoteChannel, RemoteError};
use crate::subcommand::ZcacheSubCommand;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use clap::{Arg, SubCommand};
use log::*;
use num_traits::cast::ToPrimitive;
use std::cmp::max;
use std::thread::sleep;
use std::time::Duration;
use util::flush_stdout;
use util::message::TYPE_ZCACHE_STATS;
use util::nice_number_count;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::*;

static NAME: &str = "stats";

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
            write_stdout!("{:0}\t", value);
        } else if value == 0 {
            // Intentionally avoid displaying "0.00B" when 0
            write_stdout!("{:>6}  ", "0");
        } else {
            write_stdout!("{:>6}  ", nice_p2size(value));
        }
    }

    fn display_count(&self, count: f64) {
        if self.show_exact_values {
            let value: u64 = count.round().to_u64().unwrap();
            write_stdout!("{:0}\t", value);
        } else if count == 0.0 {
            // Intentionally avoid displaying "0.00" when 0
            write_stdout!("{:>6}  ", "0");
        } else {
            write_stdout!("{:>6}  ", nice_number_count(count));
        }
    }

    fn display_percent(&self, value: f64, total: f64) {
        let percent = if total == 0.0 {
            0.0
        } else {
            (value * 100.0) / total
        };

        if self.show_exact_values {
            write_stdout!("{:0.0}\t", percent.round());
        } else if !(0.05..99.95).contains(&percent) {
            write_stdout!("{:>5.0}%  ", percent.round());
        } else {
            write_stdout!("{:>5.1}%  ", percent);
        }
    }

    fn display_timestamp(&self) {
        if self.show_exact_values {
            // e.g. "1639893294"
            write_stdout!("{}", Local::now().format("%s%t"));
        } else {
            let time = if self.interval_is_subsecond {
                // e.g. "05:43:54.254"
                Local::now().format("%H:%M:%S%.3f")
            } else {
                // e.g. "05:43:54"
                Local::now().format("%H:%M:%S")
            };
            write_stdout!("{0:>1$}  ", time, self.time_width());
        }
    }

    fn display_dashes(width: usize) {
        write_stdout!("{0:-<1$}  ", "-", width);
    }

    fn display_headers_impl(&self, top: Vec<(&str, usize)>, bottom: Vec<&str>) {
        if self.show_time {
            write_stdout!("{0:^1$}  ", "TIMESTAMP", self.time_width());
        }
        for (header, n) in top.iter() {
            let width = (n * (StatsDisplay::VALUE_WIDTH + 2)) - 2;
            write_stdout!("{0:^1$}  ", header, width);
        }
        writeln_stdout!();

        if self.show_time {
            write_stdout!(
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
            write_stdout!("{0:^1$}{2:3$}", h, StatsDisplay::VALUE_WIDTH, "", spacing);
        }
        writeln_stdout!();

        // Now print dashes underneath each column
        if self.show_time {
            StatsDisplay::display_dashes(self.time_width());
        }

        for _h in 0..bottom.len() {
            StatsDisplay::display_dashes(StatsDisplay::VALUE_WIDTH);
        }
        writeln_stdout!();
    }

    fn display_headers(&self) {
        // Produces header output like below. There can be additional opt-in headers.
        //
        // TIMESTAMP    CACHE-LOOKUP     CACHE-MISS     CACHE-INSERT
        // 2022-01-12  count   bytes   count   ratio   count   bytes
        // ----------  ------  ------  ------  ------  ------  ------

        // Top headers is a vector of tuples: (header-title, column-count)
        let mut top_header: Vec<(&str, usize)> =
            vec![("CACHE-LOOKUP", 2), ("CACHE-HITS", 2), ("CACHE-INSERT", 2)];
        // Bottom headers is a vector of: header-column-name
        let mut bottom_header = vec!["count", "bytes", "count", "ratio", "count", "bytes"];

        if self.show_lookup_detail {
            // Slot in right after "CACHE-LOOKUP" column
            top_header.insert(1, ("--------INDEX-ACCESS--------", 4));
            bottom_header.insert(2, "pendch");
            bottom_header.insert(3, "entry$");
            bottom_header.insert(4, "chunk$");
            bottom_header.insert(5, "disk");
        }

        // the following optional headers are appended in the order presented here

        if self.show_insert_detail {
            top_header.append(&mut vec![("INSERT-SOURCE", 3), ("INSERT-DROPS", 2)]);
            bottom_header.append(&mut vec!["read", "write", "spec-r", "full-q", "lkbusy"]);
        }

        if self.show_extended {
            top_header.append(&mut vec![("BUF-BYTES-USED", 2)]);
            bottom_header.append(&mut vec!["demand", "spec"]);
            top_header.append(&mut vec![("CACHE-OTHER", 3)]);
            bottom_header.append(&mut vec!["evicts", "pending", "healed"]);
        }

        if self.show_block_allocator {
            // Append after all other columns
            top_header.append(&mut vec![("ALLOCATOR", 2), ("ALLOCATOR-FREE", 2)]);
            bottom_header.append(&mut vec!["alloc", "avail", "space", "slabs"]);
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

        // TIMESTAMP (optional)
        if self.show_time {
            self.display_timestamp();
        }

        // LOOKUPS
        let write_lookups = values.value(LookupForWrite) as f64 * scale;
        let read_lookups = values.value(LookupForRead) as f64 * scale;
        let pending_changes_hits = values.value(IndexHitPendingChanges) as f64 * scale;
        let index_cache_hits = values.value(IndexHitIndexCache) as f64 * scale;
        let chunk_cache_hits = values.value(IndexHitChunkCache) as f64 * scale;
        let disk_hits = values.value(IndexHitDisk) as f64 * scale;
        let hits = values.value(CacheHit) as f64 * scale;
        let total_lookups = read_lookups + write_lookups;

        self.display_count(total_lookups);
        self.display_bytes(values.value(LookupBytes) as f64 * scale);
        if self.show_lookup_detail {
            // LOOKUP DETAILS (optional)
            self.display_percent(pending_changes_hits, read_lookups);
            self.display_percent(index_cache_hits, read_lookups);
            self.display_percent(chunk_cache_hits, read_lookups);
            self.display_percent(disk_hits, read_lookups);
        }
        // HITS
        self.display_count(hits);
        self.display_percent(hits, read_lookups);

        // INSERTS
        let inserts = (values.value(InsertForRead)
            + values.value(InsertForWrite)
            + values.value(InsertForSpeculativeRead)
            + values.value(InsertForHealing)) as f64
            * scale;
        self.display_count(inserts);
        self.display_bytes(values.value(InsertBytes) as f64 * scale);

        // INSERT DETAILS (optional)
        if self.show_insert_detail {
            self.display_percent(values.value(InsertForRead) as f64 * scale, inserts);
            self.display_percent(values.value(InsertForWrite) as f64 * scale, inserts);
            self.display_percent(
                values.value(InsertForSpeculativeRead) as f64 * scale,
                inserts,
            );
            self.display_count(values.value(InsertDropQueueFull) as f64 * scale);
            self.display_count(values.value(InsertDropLockBusy) as f64 * scale);
        }

        // EXTENDED (optional)
        if self.show_extended {
            self.display_bytes(values.value(DemandBufferBytesAvailable) as f64);
            self.display_bytes(values.value(SpeculativeBufferBytesAvailable) as f64);
            self.display_count(values.value(Evictions) as f64 * scale);
            // Note - PendingChanges stat is instantaneous so no need to scale
            self.display_count(values.value(PendingChanges) as f64);
            self.display_count(values.value(HealedBlocks) as f64 * scale);
        }

        // BLOCK-ALLOCATOR (optional)
        if self.show_block_allocator {
            let block_allocator_size = values.value(BlockAllocatorSize);
            let free_slabs_size = values.value(BlockAllocatorFreeSlabsSize);
            let block_allocator_free = values.value(BlockAllocatorAvailable);
            let block_allocator_allocated = block_allocator_size - block_allocator_free;

            self.display_bytes(block_allocator_allocated as f64);
            self.display_bytes(block_allocator_free as f64);
            self.display_percent(block_allocator_free as f64, block_allocator_size as f64);
            self.display_percent(free_slabs_size as f64, block_allocator_size as f64);
        }

        writeln_stdout!();
    }

    async fn display_stats(&self) -> Result<()> {
        let mut iteration = 0;
        let mut previous = CacheStats::default(); // empty stats
        let mut remote = RemoteChannel::new(false).await?;

        loop {
            let latest: CacheStats;

            match remote.call(TYPE_ZCACHE_STATS, None).await {
                Ok(response) => {
                    let stats_json = response.lookup_string("stats_json").unwrap();
                    latest = serde_json::from_str(stats_json.to_str()?).unwrap();
                }
                Err(RemoteError::ResultError(_)) => {
                    return Err(anyhow!("No cache found"));
                }
                Err(RemoteError::Other(e)) => {
                    writeln_stdout!("remote call error: {}", e);
                    // typically something like "Connection reset by peer (os error 104)"
                    return Err(e);
                }
            }

            // Periodically display the column headers
            if (iteration % (self.get_terminal_height() - 3) as u64) == 0 {
                self.display_headers();
            }

            if previous.cache_runtime_id.is_nil() {
                // Initial empty previous needs to match (so we get first line totals)
                previous.cache_runtime_id = latest.cache_runtime_id;
            }

            if latest.cache_runtime_id == previous.cache_runtime_id {
                // Display the net values of collected stats
                self.display_stat_values(&(&latest - &&previous));
            } else {
                info!("object agent restarted");
            }

            // Flush stdout in case output is redirected to a file
            flush_stdout().ok();

            match self.interval {
                None => return Ok(()),
                Some(duration) => {
                    previous = latest;
                    iteration += 1;
                    if let Some(count) = self.count {
                        if iteration >= count {
                            return Ok(());
                        }
                    }
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
            .after_help(
                "TERMINOLOGY:\n\
                When we look for a block in the zettacache, we must first look\n\
                in the index to determine if it's present and if so where it is\n\
                on disk.  The following layers of caching are checked in order:\n\
                \n\
                pendch: index entry found in pending changes (new blocks or atime \
                        updates not yet reflected in main on-disk index)\n\
                entry$: index entry found in the entry cache\n\
                chunk$: entry (or lack thereof) found in chunk cache\n\
                  disk: a chunk of the main index was read from disk to find this \
                        entry (or lack thereof)\n\
                ",
            )
            .arg(
                Arg::with_name("timestamp")
                    .long("timestamp")
                    .short("t")
                    .help("Display a timestamp on each line of stats"),
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
