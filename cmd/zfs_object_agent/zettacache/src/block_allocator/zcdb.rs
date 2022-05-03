use std::cmp;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use util::nice_p2size;
use util::writeln_stdout;
use util::From64;

use super::BlockAllocatorPhys;
use super::Slab;
use super::SlabBucketSize;
use super::SlabEnum;
use super::Slabs;
use crate::block_access::BlockAccess;
use crate::block_allocator::SlabId;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::DumpSlabsOptions;

pub async fn zcachedb_dump_spacemaps(
    phys: BlockAllocatorPhys,
    block_access: Arc<BlockAccess>,
    slab_builder: &SlabAllocatorBuilder,
) {
    let import_cb = |entry| writeln_stdout!("{entry:?}");

    writeln_stdout!("DUMP SPACEMAP");
    writeln_stdout!("{:?}", phys.spacemap);
    phys.spacemap
        .load(block_access.clone(), slab_builder.access(), import_cb)
        .await;
    writeln_stdout!();

    writeln_stdout!("DUMP SPACEMAP_NEXT");
    writeln_stdout!("{:?}", phys.spacemap_next);
    phys.spacemap_next
        .load(block_access.clone(), slab_builder.access(), import_cb)
        .await;
}

struct AllocationBucketStatistics {
    pub nslabs: u64,
    pub free_space: u64,
    pub slab_size: u64,
    pub total_segments: u64,
}

impl AllocationBucketStatistics {
    fn new(slab_size: u64) -> AllocationBucketStatistics {
        AllocationBucketStatistics {
            nslabs: 0,
            free_space: 0,
            slab_size,
            total_segments: 0,
        }
    }

    fn add_slab(&mut self, slab: &Slab) {
        self.total_segments += slab.num_segments();
        self.free_space += slab.free_space();
        self.nslabs += 1;
    }

    fn total_space(&self) -> u64 {
        self.slab_size * self.nslabs
    }

    fn allocated_space(&self) -> u64 {
        self.total_space() - self.free_space
    }

    fn capacity_perc(&self) -> u64 {
        (self.allocated_space() * 100) / self.total_space()
    }

    fn segments_per_slab(&self) -> u64 {
        self.total_segments / self.nslabs
    }

    fn stackgraph(&self, hist_scaling_factor: u64) -> String {
        if self.nslabs == 0 {
            return "".to_string();
        }
        let hist_factor = cmp::max(usize::from64(hist_scaling_factor), REPORT_HISTOGRAM_WIDTH);
        let hist_slots = (usize::from64(self.nslabs) * REPORT_HISTOGRAM_WIDTH) / hist_factor;
        let free_slots = ((usize::from64(self.free_space) * hist_slots)
            + usize::from64(self.total_space() / 2 - 1))
            / usize::from64(self.total_space());

        format!(
            "{}{}",
            "*".repeat(hist_slots - free_slots),
            "=".repeat(free_slots),
        )
    }
}

impl fmt::Display for AllocationBucketStatistics {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:>6} {:>7} {:>7} {:>7} {:>3}% {:>6}",
            self.nslabs,
            nice_p2size(self.total_space()),
            nice_p2size(self.allocated_space()),
            nice_p2size(self.free_space),
            if self.nslabs != 0 {
                self.capacity_perc().to_string()
            } else {
                "-".to_string()
            },
            if self.nslabs != 0 {
                self.segments_per_slab().to_string()
            } else {
                "-".to_string()
            },
        )
    }
}

struct AllocationBucketInfo {
    extent_based: bool,
    max_size: u32,
    stats: AllocationBucketStatistics,
    slabs_by_freeness: BTreeSet<(u64, SlabId)>,
}

impl AllocationBucketInfo {
    fn new(extent_based: bool, max_size: u32, slab_size: u64) -> AllocationBucketInfo {
        AllocationBucketInfo {
            extent_based,
            stats: AllocationBucketStatistics::new(slab_size),
            max_size,
            slabs_by_freeness: BTreeSet::default(),
        }
    }

    fn add_slab(&mut self, slab: &Slab) {
        self.stats.add_slab(slab);
        self.slabs_by_freeness.insert((slab.free_space(), slab.id));
    }

    // Given a permille value (1000-quantile) for the number of slabs in this
    // bucket, this function returns the following tuple (allocated_bytes of
    // those slabs, capacity ratio [e.g. allocated over total space] of those
    // slabs)
    fn allocated_quantile(&self, permille: u64) -> (u64, f64) {
        let mut allocated_bytes = 0;
        let slabs_to_visit = cmp::max(
            (usize::from64(permille) * self.slabs_by_freeness.len()) / 1000,
            1,
        );
        for (free_space, _) in self.slabs_by_freeness.iter().rev().take(slabs_to_visit) {
            allocated_bytes += self.stats.slab_size - free_space;
        }
        let total_space = (slabs_to_visit as u64) * self.stats.slab_size;
        (
            allocated_bytes,
            (allocated_bytes as f64) / total_space as f64,
        )
    }
}

impl fmt::Display for AllocationBucketInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} {:>8}: {}",
            if self.extent_based { "*" } else { " " },
            nice_p2size(u64::from(self.max_size)),
            self.stats
        )
    }
}

struct SlabBucketsReport {
    buckets: BTreeMap<SlabBucketSize, AllocationBucketInfo>,
    total: AllocationBucketStatistics,
    hist_scaling_factor: u64,
}

const REPORT_HISTOGRAM_WIDTH: usize = 39;

impl SlabBucketsReport {
    fn new(buckets: &[(SlabBucketSize, bool)], slab_size: u64) -> SlabBucketsReport {
        SlabBucketsReport {
            buckets: buckets
                .iter()
                .map(|(max_size, is_extent_based)| {
                    (
                        *max_size,
                        AllocationBucketInfo::new(*is_extent_based, max_size.0, slab_size),
                    )
                })
                .collect(),
            total: AllocationBucketStatistics::new(slab_size),
            hist_scaling_factor: 0,
        }
    }

    fn reset_hist_scaling_factor(&mut self, count: u64) {
        self.hist_scaling_factor = cmp::max(self.hist_scaling_factor, count);
    }

    fn add_slab(&mut self, slab: &Slab) {
        // Evacuating slabs don't belong on a bucket, just log them for the total stats
        match slab.inner {
            SlabEnum::BitmapBased(_) | SlabEnum::ExtentBased(_) => {
                let bucket_info = self
                    .buckets
                    .range_mut(SlabBucketSize(slab.max_size())..)
                    .next()
                    .unwrap()
                    .1;
                bucket_info.add_slab(slab);
                let nslabs = bucket_info.stats.nslabs;
                self.reset_hist_scaling_factor(nslabs);
            }
            SlabEnum::Evacuating(_) => {}
        }
        self.total.add_slab(slab);
    }

    fn dump_report(&self, verbosity: u64) {
        writeln_stdout!("E MAX_SIZE:  NSLAB    SIZE   ALLOC    FREE  CAP  SEG/S NSLAB");
        for bucket in self.buckets.values() {
            writeln_stdout!(
                "{} {}",
                bucket,
                bucket.stats.stackgraph(self.hist_scaling_factor)
            );

            if verbosity > 0 && bucket.stats.nslabs > 0 {
                const CAP_BUCKET_PERCENTAGE_RANGE: usize = 10;
                let mut cap_hist = [0u64; CAP_BUCKET_PERCENTAGE_RANGE];
                let mut max_count = 0;
                for (slab_free_space, _) in bucket.slabs_by_freeness.iter() {
                    let perc_cap = usize::from64(
                        ((bucket.stats.slab_size - slab_free_space) * 100) / bucket.stats.slab_size,
                    );
                    let idx = if perc_cap == 100 {
                        9
                    } else {
                        perc_cap / CAP_BUCKET_PERCENTAGE_RANGE
                    };
                    cap_hist[idx] += 1;
                    max_count = cmp::max(max_count, usize::from64(cap_hist[idx]));
                }
                max_count = cmp::max(max_count, REPORT_HISTOGRAM_WIDTH);

                let (perm_1_bytes, perm_1_perc) = bucket.allocated_quantile(1);
                let (perm_10_bytes, perm_10_perc) = bucket.allocated_quantile(10);
                let (perm_100_bytes, perm_100_perc) = bucket.allocated_quantile(100);

                writeln_stdout!("\t%CAP: NSLABS");
                for (idx, count) in cap_hist.iter().enumerate() {
                    writeln_stdout!(
                        "\t{:>4}: {} {}",
                        idx * CAP_BUCKET_PERCENTAGE_RANGE,
                        "*".repeat((usize::from64(*count) * REPORT_HISTOGRAM_WIDTH) / max_count),
                        *count
                    );
                }
                writeln_stdout!("\t-------------");
                writeln_stdout!(
                    "\tallocated space in 0.1% of free-est slabs: {} ({:.1}%)",
                    nice_p2size(perm_1_bytes),
                    perm_1_perc * 100.0
                );
                writeln_stdout!(
                    "\tallocated space in   1% of free-est slabs: {} ({:.1}%)",
                    nice_p2size(perm_10_bytes),
                    perm_10_perc * 100.0
                );
                writeln_stdout!(
                    "\tallocated space in  10% of free-est slabs: {} ({:.1}%)",
                    nice_p2size(perm_100_bytes),
                    perm_100_perc * 100.0
                );
                writeln_stdout!("\t-------------");
            }
        }
    }
}

fn zcachedb_dump_slabs_print_legend() {
    writeln_stdout!("============================================================");
    writeln_stdout!("E: Extent-based");
    writeln_stdout!("MAX_SIZE: largest allocation that can be made to these slabs");
    writeln_stdout!("NSLAB: number of slabs");
    writeln_stdout!("SIZE: total bytes in slabs (ALLOC + FREE)");
    writeln_stdout!("ALLOC: allocated bytes in slabs");
    writeln_stdout!("FREE: free (available) bytes in slabs");
    writeln_stdout!("CAP: percent allocated (ALLOC / SIZE)");
    writeln_stdout!("SEG/S: average number of disjoint free segments per slab");
    writeln_stdout!("============================================================");
    writeln_stdout!();
}

pub async fn zcachedb_dump_slabs(
    block_access: Arc<BlockAccess>,
    slab_builder: &mut SlabAllocatorBuilder,
    phys: BlockAllocatorPhys,
    opts: DumpSlabsOptions,
) {
    let slab_size = slab_builder.slab_size();
    let buckets = phys.slab_buckets.buckets.clone();
    let mut cache_slabs = vec![];
    let mut slabs_per_device = HashMap::new();
    for disk in block_access.disks() {
        slabs_per_device.insert(disk, vec![]);
    }
    let slabs = Slabs::open(
        block_access.clone(),
        slab_builder,
        &phys.spacemap,
        &phys.spacemap_next,
    )
    .await;

    for slab in slabs.iter() {
        if opts.verbosity > 1 {
            slab.dump_info();
        }
        cache_slabs.push(slab);
        slabs_per_device
            .get_mut(&slab.location().disk())
            .unwrap()
            .push(slab);
    }

    zcachedb_dump_slabs_print_legend();
    for (disk, device_slabs) in slabs_per_device {
        writeln_stdout!("============================================================");
        writeln_stdout!("=                        {}", block_access.disk_path(disk));
        writeln_stdout!("============================================================");
        zcachedb_dump_slabs_report(&device_slabs, slab_size, &buckets, &opts)
    }
    writeln_stdout!("============================================================");
    writeln_stdout!("=                        whole cache");
    writeln_stdout!("============================================================");
    zcachedb_dump_slabs_report(&cache_slabs, slab_size, &buckets, &opts);
}

fn zcachedb_dump_slabs_report(
    slabs: &[&Slab],
    slab_size: u64,
    buckets: &[(SlabBucketSize, bool)],
    opts: &DumpSlabsOptions,
) {
    let mut buckets_by_max_size = SlabBucketsReport::new(buckets, slab_size);
    let bitmap_summary_dist: Vec<(SlabBucketSize, bool)> = [1, 2, 4, 8, 16]
        .iter()
        .map(|kbytes| (SlabBucketSize(kbytes * 1024u32), false))
        .collect();
    let mut bitmap_based_summary = SlabBucketsReport::new(&bitmap_summary_dist, slab_size);
    let mut extent_summary_dist: Vec<(SlabBucketSize, bool)> = [64, 256, 1024]
        .iter()
        .map(|kbytes| (SlabBucketSize(kbytes * 1024u32), true))
        .collect();
    extent_summary_dist.push((SlabBucketSize(u32::try_from(slab_size).unwrap()), true));
    let mut extent_based_summary = SlabBucketsReport::new(&extent_summary_dist, slab_size);
    let mut evacuating_total = AllocationBucketStatistics::new(slab_size);

    for slab in slabs {
        buckets_by_max_size.add_slab(slab);

        match &slab.inner {
            SlabEnum::BitmapBased(_) => bitmap_based_summary.add_slab(slab),
            SlabEnum::ExtentBased(_) => extent_based_summary.add_slab(slab),
            SlabEnum::Evacuating(_) => evacuating_total.add_slab(slab),
        }
    }
    let max_scaling_factor = cmp::max(
        bitmap_based_summary.hist_scaling_factor,
        extent_based_summary.hist_scaling_factor,
    );
    bitmap_based_summary.reset_hist_scaling_factor(max_scaling_factor);
    extent_based_summary.reset_hist_scaling_factor(max_scaling_factor);
    buckets_by_max_size.reset_hist_scaling_factor(max_scaling_factor);

    buckets_by_max_size.dump_report(opts.verbosity);
    writeln_stdout!();
    writeln_stdout!("~~~~~~~~~~~~~~~~~~~~~~~~  SUMMARY  ~~~~~~~~~~~~~~~~~~~~~~~~~");
    bitmap_based_summary.dump_report(opts.verbosity);
    writeln_stdout!("------------------------------------------------------------");
    writeln_stdout!("    BITMAP: {}", bitmap_based_summary.total);
    writeln_stdout!();
    extent_based_summary.dump_report(opts.verbosity);
    writeln_stdout!("------------------------------------------------------------");
    writeln_stdout!("    EXTENT: {}", extent_based_summary.total);
    writeln_stdout!("------------------------------------------------------------");
    writeln_stdout!("EVACUATING: {}", evacuating_total);
    writeln_stdout!("============================================================");
    writeln_stdout!("E MAX_SIZE:  NSLAB    SIZE   ALLOC    FREE  CAP  SEG/S NSLAB");
    writeln_stdout!("     TOTAL: {}", buckets_by_max_size.total);
    writeln_stdout!();
}
