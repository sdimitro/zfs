//! zcache stats subcommand

use std::cmp::max;
use std::thread::sleep;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use clap::Parser;
use log::*;
use num_traits::cast::ToPrimitive;
use util::flush_stdout;
use util::message::TYPE_ZCACHE_STATS;
use util::nice_number_count;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
use util::zettacache_stats::CacheStatCounter::*;
use util::zettacache_stats::*;

use crate::remote_channel::RemoteChannel;
use crate::remote_channel::RemoteError;
use crate::subcommand::ZcacheSubCommand;

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
            write_stdout!("{:>6} ", "0");
        } else {
            write_stdout!("{:>6} ", nice_p2size(value));
        }
    }

    fn display_count(&self, count: f64) {
        if self.show_exact_values {
            let value: u64 = count.round().to_u64().unwrap();
            write_stdout!("{:0}\t", value);
        } else if count == 0.0 {
            // Intentionally avoid displaying "0.00" when 0
            write_stdout!("{:>6} ", "0");
        } else {
            write_stdout!("{:>6} ", nice_number_count(count));
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
            write_stdout!("{:>5.0}% ", percent.round());
        } else {
            write_stdout!("{:>5.1}% ", percent);
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
            write_stdout!("{0:>1$} ", time, self.time_width());
        }
    }

    fn display_dashes(width: usize) {
        write_stdout!("{0:-<1$} ", "-", width);
    }

    fn display_headers_impl(&self, top: Vec<(&str, usize)>, bottom: Vec<&str>) {
        if self.show_time {
            write_stdout!("{0:^1$} ", "TIMESTAMP", self.time_width());
        }
        for (header, n) in top.iter() {
            let width = (n * (StatsDisplay::VALUE_WIDTH + 1)) - 1;
            write_stdout!("{0:^1$} ", header, width);
        }
        writeln_stdout!();

        if self.show_time {
            write_stdout!(
                "{0:>1$} ",
                format!("{}", Local::now().format("%Y-%m-%d")),
                self.time_width()
            );
        }
        for h in bottom.iter() {
            let spacing = if h.len() > StatsDisplay::VALUE_WIDTH {
                // Adjust spacing to accommodate column headers that are > 6 characters
                h.len() - StatsDisplay::VALUE_WIDTH
            } else {
                1 // default is one space
            };
            write_stdout!("{0:>1$}{2:3$}", h, StatsDisplay::VALUE_WIDTH, "", spacing);
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
        //    LOOKUPS     ----HITS---     INSERTS
        // count  bytes  count  ratio  count  bytes
        // ------ ------ ------ ------ ------ ------

        // Top headers is a vector of tuples: (header-title, column-count)
        let mut top_header: Vec<(&str, usize)> =
            vec![("LOOKUP", 1), ("--------HITS--------", 3), ("INSERTS", 2)];
        // Bottom headers is a vector of: header-column-name
        let mut bottom_header = vec!["count", "count", "bytes", "ratio", "count", "bytes"];

        if self.show_lookup_detail {
            // Slot in right after "CACHE-LOOKUP" column
            top_header.insert(1, ("--------INDEX-ACCESS-------", 4));
            bottom_header.insert(1, "pendch");
            bottom_header.insert(2, "entry$");
            bottom_header.insert(3, "chunk$");
            bottom_header.insert(4, "disk");
        }

        // the following optional headers are appended in the order presented here

        if self.show_insert_detail {
            top_header.append(&mut vec![("INSERT-SOURCE", 3), ("INSERT-DROPS", 2)]);
            bottom_header.append(&mut vec!["read", "write", "spec-r", "buffer", "alloc"]);
        }

        if self.show_extended {
            top_header.append(&mut vec![("BUFFER-USED", 2)]);
            bottom_header.append(&mut vec!["demand", "spec"]);
            top_header.append(&mut vec![("OTHER", 2)]);
            bottom_header.append(&mut vec!["evicts", "pendch"]);
        }

        if self.show_block_allocator {
            // Append after all other columns
            top_header.append(&mut vec![("BLOCKS", 2), ("FREE-SPACE", 2)]);
            bottom_header.append(&mut vec!["alloc", "avail", "blocks", "slabs"]);
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
        let lookups = values.value(Lookup) as f64 * scale;
        self.display_count(lookups);
        // LOOKUP DETAILS (optional)
        if self.show_lookup_detail {
            self.display_percent(values.value(IndexHitPendingChanges) as f64 * scale, lookups);
            self.display_percent(values.value(IndexHitIndexCache) as f64 * scale, lookups);
            self.display_percent(values.value(IndexHitChunkCache) as f64 * scale, lookups);
            self.display_percent(values.value(IndexHitDisk) as f64 * scale, lookups);
        }

        // HITS
        let hits = values.value(CacheHit) as f64 * scale;
        self.display_count(hits);
        self.display_bytes(values.value(CacheHitBytes) as f64 * scale);
        self.display_percent(hits, lookups);

        // INSERTS
        let inserts = (values.value(InsertForRead)
            + values.value(InsertForWrite)
            + values.value(InsertForSpeculativeRead)
            + values.value(InsertForHeal)) as f64
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
            self.display_count(values.value(InsertDropBufferFull) as f64 * scale);
            self.display_count(values.value(InsertDropCacheFull) as f64 * scale);
        }

        // EXTENDED (optional)
        if self.show_extended {
            self.display_bytes(values.value(DemandBufferBytesAvailable) as f64);
            self.display_bytes(values.value(SpeculativeBufferBytesAvailable) as f64);
            self.display_count(values.value(Evictions) as f64 * scale);
            // Note - PendingChanges stat is instantaneous so no need to scale
            self.display_count(values.value(PendingChanges) as f64);
        }

        // BLOCK-ALLOCATOR (optional)
        if self.show_block_allocator {
            let available_space = values.value(AvailableSpace);
            let slab_capacity = values.value(SlabCapacity);
            let free_blocks_size = values.value(AvailableBlocksSize);
            let free_slabs_size = values.value(AvailableSlabsSize);
            let block_allocator_allocated = slab_capacity - free_blocks_size - free_slabs_size;

            self.display_bytes(block_allocator_allocated as f64);
            self.display_bytes(available_space as f64);
            self.display_percent(free_blocks_size as f64, slab_capacity as f64);
            self.display_percent(free_slabs_size as f64, slab_capacity as f64);
        }

        writeln_stdout!();
    }

    async fn display_stats(&self) -> Result<()> {
        let mut iteration = 0;
        let mut previous = CacheStats::default(); // empty stats
        let mut remote = RemoteChannel::new(false).await?;

        loop {
            let latest: CacheStats = match remote.call(TYPE_ZCACHE_STATS, None).await {
                Ok(response) => {
                    let stats_json = response.lookup_string("stats_json").unwrap();
                    serde_json::from_str(stats_json.to_str()?).unwrap()
                }
                Err(RemoteError::ResultError(_)) => {
                    return Err(anyhow!("No cache found"));
                }
                Err(RemoteError::Other(e)) => {
                    writeln_stdout!("remote call error: {}", e);
                    // typically something like "Connection reset by peer (os error 104)"
                    return Err(e);
                }
            };

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

#[derive(Parser)]
#[clap(about = "Display cache statistics.")]
#[clap(after_help = "TERMINOLOGY:\n\
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
    ")]
pub struct Stats {
    /// Display a timestamp on each line of stats
    #[clap(short = 't', long)]
    timestamp: bool,

    /// Display additional cache insert details
    #[clap(short = 'i', long, conflicts_with = "all")]
    insert_detail: bool,

    /// Display additional cache lookup details
    #[clap(short = 'l', long, conflicts_with = "all")]
    lookup_detail: bool,

    /// Display additional block allocator details
    #[clap(short = 'b', long, conflicts_with = "all")]
    block_allocator: bool,

    /// Display extended statistics
    #[clap(short = 'x', long, conflicts_with = "all")]
    extended: bool,

    /// Display numbers in parsable (exact) values
    #[clap(short = 'p', long)]
    parsable: bool,

    /// Display all possible columns (i.e. alias to -biltx)
    #[clap(short = 'a', long)]
    all: bool,

    /// Statistics are printed every <interval> seconds"
    #[clap()]
    // XXX could this be a duration here?
    interval: Option<f64>,

    /// Stop after <count> reports have been displayed
    #[clap()]
    count: Option<u64>,
}

#[async_trait]
impl ZcacheSubCommand for Stats {
    async fn invoke(&self) -> Result<()> {
        let interval = self.interval.map(Duration::from_secs_f64);

        StatsDisplay {
            show_time: self.all || self.timestamp,
            show_extended: self.all || self.extended,
            show_insert_detail: self.all || self.insert_detail,
            show_lookup_detail: self.all || self.lookup_detail,
            show_block_allocator: self.all || self.block_allocator,
            show_exact_values: self.parsable,
            interval_is_subsecond: interval.map_or(false, |d| d.as_secs() < 1),
            interval,
            count: self.count,
        }
        .display_stats()
        .await
    }
}
