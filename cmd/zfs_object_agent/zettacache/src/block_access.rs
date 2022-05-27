use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::unix::prelude::AsRawFd;
use std::os::unix::prelude::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::RwLock;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use bincode::Options;
use bytesize::ByteSize;
use derivative::Derivative;
use futures::Future;
use libc::c_void;
use log::*;
use nix::errno::Errno;
use nix::sys::stat::SFlag;
use num_traits::Num;
use num_traits::NumCast;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use util::from64::AsUsize;
use util::iter_wrapping;
use util::measure;
use util::message::ExpandDiskResponse;
use util::serde::from_json_slice;
use util::tunable;
use util::with_alloctag;
use util::zettacache_stats::*;
use util::AlignedBytes;
use util::AlignedVec;
use util::DeviceEntry;
use util::DeviceList;
use util::From64;
use uuid::Uuid;

use crate::base_types::DiskId;
use crate::base_types::DiskLocation;
use crate::base_types::Extent;

tunable! {
    static ref MIN_SECTOR_SIZE: usize = 512;
    static ref DISK_WRITE_MAX_QUEUE_DEPTH: usize = 32;
    // Stop aggregating if run would exceed DISK_WRITE_MAX_AGGREGATION_SIZE
    pub static ref DISK_WRITE_MAX_AGGREGATION_SIZE: ByteSize = ByteSize::kib(128);
    // CHUNK must be > MAX_AGG_SIZE, see Disk::write()
    static ref DISK_WRITE_CHUNK: ByteSize = ByteSize::mib(1);
    static ref DISK_WRITE_QUEUE_EMPTY_DELAY: Duration = Duration::from_millis(1);
    static ref DISK_METADATA_WRITE_MAX_QUEUE_DEPTH: usize = 16;
    pub static ref DISK_READ_MAX_QUEUE_DEPTH: usize = 64;
}

#[derive(Serialize, Deserialize, Debug)]
struct BlockHeader {
    #[serde(rename = "p")]
    #[serde(alias = "payload_size")]
    payload_size: usize,
    #[serde(rename = "e")]
    #[serde(alias = "encoding")]
    encoding: EncodeType,
    #[serde(rename = "c")]
    #[serde(alias = "compression")]
    compression: CompressType,
    #[serde(rename = "k")]
    #[serde(alias = "checksum")]
    checksum: u64,
}

#[must_use]
struct OpInProgress<'a> {
    begin: Instant,
    counters: &'a IoStatValues,
}

impl<'a> OpInProgress<'a> {
    fn new(counters: &'a IoStatValues) -> Self {
        counters.active_count.0.fetch_add(1, Ordering::Relaxed);
        OpInProgress {
            begin: Instant::now(),
            counters,
        }
    }

    fn end(self, bytes: u64) {
        let counters = self.counters;
        counters.operations.0.fetch_add(1, Ordering::Relaxed);
        counters.total_bytes.0.fetch_add(bytes, Ordering::Relaxed);
        counters.total_nanoseconds.0.fetch_add(
            self.begin.elapsed().as_nanos().try_into().unwrap(),
            Ordering::Relaxed,
        );

        // The first latency bucket is 1 microsecond
        let latency = self.begin.elapsed().as_micros();
        let histo = &counters.latency_histogram.0;
        histo
            .get(latency.next_power_of_two().trailing_zeros() as usize)
            .unwrap_or_else(|| histo.last().unwrap())
            .0
            .fetch_add(1, Ordering::Relaxed);

        // The first request size bucket is 512 bytes
        let size = bytes >> 9;
        let histo = &counters.request_histogram.0;
        histo
            .get(size.next_power_of_two().trailing_zeros() as usize)
            .unwrap_or_else(|| histo.last().unwrap())
            .0
            .fetch_add(1, Ordering::Relaxed);
    }
}

impl<'a> Drop for OpInProgress<'a> {
    fn drop(&mut self) {
        self.counters.active_count.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct BlockAccess {
    sector_size: usize,
    disks: RwLock<Vec<Disk>>,
    readonly: bool,
    timebase: Instant,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Disk {
    // We want all the reader/writer_threads to share the same file descriptor, but we don't have
    // a mechanism to ensure that they stop using the fd when the DiskStruct is dropped and the
    // fd is closed.  To solve this we simply never close the fd.  The fd is owned by the File,
    // and we leave a reference to it here to indicate that it's related to this Disk, even
    // though it's only used via the reader/writer_threads.
    #[allow(dead_code)]
    file: &'static File,

    path: PathBuf,
    canonical_path: PathBuf,
    size: Mutex<u64>,
    sector_size: usize,
    #[derivative(Debug = "ignore")]
    io_stats: &'static DiskIoStats,
    #[derivative(Debug = "ignore")]
    reader_tx: flume::Sender<ReadMessage>,
    #[derivative(Debug = "ignore")]
    writer_txs: Vec<mpsc::UnboundedSender<WriteMessage>>,
    #[derivative(Debug = "ignore")]
    metadata_writer_txs: Vec<mpsc::UnboundedSender<WriteMessage>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum CompressType {
    #[serde(rename = "N")]
    #[serde(alias = "None")]
    None,
    #[serde(rename = "4")]
    #[serde(alias = "Lz4")]
    Lz4,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum EncodeType {
    #[serde(rename = "J")]
    #[serde(alias = "Json")]
    Json,
    #[serde(rename = "B")]
    #[serde(alias = "Bincode")]
    Bincode,
    #[serde(rename = "F")]
    #[serde(alias = "BincodeFixint")]
    BincodeFixint,
}

#[cfg(target_os = "linux")]
const CUSTOM_OFLAGS: i32 = libc::O_DIRECT;
#[cfg(not(target_os = "linux"))]
const CUSTOM_OFLAGS: i32 = 0;

struct ReadMessage {
    offset: u64,
    size: usize,
    io_type: DiskIoType,
    tx: oneshot::Sender<AlignedBytes>,
}

struct WriteMessage {
    offset: u64,
    bytes: AlignedBytes,
    tx: oneshot::Sender<()>,
}

impl Disk {
    pub fn new(path: &Path, readonly: bool) -> Result<Disk> {
        // Note: using std file open so that this func can be non-async.
        // Although this is blocking from a tokio thread, it's used
        // infrequently, and we're already blocking from the ioctls to get the
        // disk size and block size.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!readonly)
            .custom_flags(CUSTOM_OFLAGS)
            .open(path)
            .with_context(|| format!("opening disk {path:?}"))?;
        // see comment in `struct Disk`
        let file = &*Box::leak(Box::new(file));
        let (sector_size, size) = disk_sizes(file)?;

        let short_name = path.file_name().unwrap().to_string_lossy().to_string();
        let canonical_path = Path::new(path).canonicalize()?;

        let (reader_tx, reader_rx) = flume::unbounded();

        let mut writer_txs = Vec::new();
        let mut writer_rxs = Vec::new();
        for _ in 0..*DISK_WRITE_MAX_QUEUE_DEPTH {
            let (tx, rx) = mpsc::unbounded_channel();
            writer_txs.push(tx);
            writer_rxs.push(rx);
        }

        let mut metadata_writer_txs = Vec::new();
        let mut metadata_writer_rxs = Vec::new();
        for _ in 0..*DISK_METADATA_WRITE_MAX_QUEUE_DEPTH {
            let (tx, rx) = mpsc::unbounded_channel();
            metadata_writer_txs.push(tx);
            metadata_writer_rxs.push(rx);
        }

        let io_stats = &*Box::leak(Box::new(DiskIoStats::new(short_name)));

        let this = Disk {
            file,
            path: path.to_owned(),
            canonical_path,
            size: Mutex::new(size),
            sector_size,
            io_stats,
            reader_tx,
            writer_txs,
            metadata_writer_txs,
        };

        for _ in 0..*DISK_READ_MAX_QUEUE_DEPTH {
            let rx = reader_rx.clone();
            // note, we want to use a "std" thread here rather than
            // tokio::task::spawn_blocking() because the latter has a limit of how many
            // threads it will create (default 512)
            std::thread::spawn(move || {
                Self::reader_thread(file, io_stats, sector_size, rx);
            });
        }
        if !readonly {
            for rx in writer_rxs {
                std::thread::spawn(move || {
                    Self::writer_thread(
                        file,
                        &io_stats.stats[DiskIoType::WriteDataForInsert],
                        sector_size,
                        rx,
                    );
                });
            }
            for rx in metadata_writer_rxs {
                std::thread::spawn(move || {
                    Self::writer_thread(
                        file,
                        &io_stats.stats[DiskIoType::MaintenanceWrite],
                        sector_size,
                        rx,
                    );
                });
            }
        }
        info!("opening cache file {path:?}: {this:?}");

        Ok(this)
    }

    fn reader_thread(
        file: &'static File,
        io_stats: &'static DiskIoStats,
        sector_size: usize,
        rx: flume::Receiver<ReadMessage>,
    ) {
        while let Ok(message) = rx.recv() {
            let op = OpInProgress::new(&io_stats.stats[message.io_type]);
            let vec = measure!()
                .func(|| {
                    pread_aligned(
                        file,
                        message.offset.try_into().unwrap(),
                        message.size,
                        sector_size,
                    )
                })
                .unwrap();
            assert_eq!(
                vec.len(),
                message.size,
                "fd={}, offset={}",
                file.as_raw_fd(),
                message.offset
            );
            op.end(message.size as u64);
            message.tx.send(vec.into()).unwrap();
        }
    }

    // This is a desugared `async fn` so that it can return a Future that does not capture
    // `&self` (as indicated by the absence of `+ '_`).  That way, the caller can drop the
    // associated RwLock before `await`ing.
    fn read(
        &self,
        offset: u64,
        size: usize,
        io_type: DiskIoType,
    ) -> impl Future<Output = AlignedBytes> {
        self.verify_aligned(offset);
        self.verify_aligned(size);

        let (tx, rx) = oneshot::channel();
        let message = ReadMessage {
            offset,
            size,
            io_type,
            tx,
        };

        // note: reader_tx is unbounded, so .send() will not block
        self.reader_tx.send(message).unwrap();
        async move { measure!().fut(rx).await.unwrap() }
    }

    fn writer_thread(
        file: &'static File,
        stat_values: &'static IoStatValues,
        sector_size: usize,
        mut rx: mpsc::UnboundedReceiver<WriteMessage>,
    ) {
        /// returns (offsets, total_bytes)
        fn find_run<'a, I: Iterator<Item = (&'a u64, &'a WriteMessage)>>(
            mut iter: I,
        ) -> (Vec<u64>, usize) {
            let (mut run, mut len) = if let Some((&offset, message)) = iter.next() {
                (vec![offset], message.bytes.len())
            } else {
                return (Vec::new(), 0);
            };
            for (&offset, message) in iter {
                if len > 0 && len + message.bytes.len() > DISK_WRITE_MAX_AGGREGATION_SIZE.as_usize()
                {
                    break;
                }
                if offset == run[0] + len as u64 {
                    run.push(offset);
                    len += message.bytes.len();
                } else {
                    break;
                }
            }
            (run, len)
        }

        let mut sorted: BTreeMap<u64, WriteMessage> = BTreeMap::new();
        let mut prev_offset = 0;

        loop {
            // Look for next run of messages in sorted queue
            let (run, len) = find_run(iter_wrapping(&sorted, prev_offset));
            if run.is_empty() {
                // Nothing in `sorted`; wait for a message
                let message = match rx.blocking_recv() {
                    Some(message) => message,
                    None => return,
                };
                sorted.insert(message.offset, message);
                // Delay a bit to allow for more messages to arrive, to improve our chances of
                // aggregation.
                sleep(*DISK_WRITE_QUEUE_EMPTY_DELAY);
            } else if run.len() == 1 {
                // Run has just one block; issue this one write
                let message = sorted.remove(&run[0]).unwrap();
                let mut bytes = message.bytes;
                // Directio requires the pointer to be sector-aligned
                if bytes.alignment() % sector_size != 0 {
                    // We need to copy AlignedBytes created from a plain Bytes (e.g. ingesting
                    // from a cache miss where we read the object and then write its blocks to
                    // the zettacache).
                    bytes = AlignedBytes::copy_from_slice(&bytes, sector_size);
                };
                assert_eq!(bytes.alignment() % sector_size, 0);
                assert_eq!(bytes.as_ptr() as usize % sector_size, 0);
                let op = OpInProgress::new(stat_values);
                nix::sys::uio::pwrite(
                    file.as_raw_fd(),
                    &bytes,
                    i64::try_from(message.offset).unwrap(),
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "pwrite(fd={} off={} len={}) failed: {}",
                        file.as_raw_fd(),
                        message.offset,
                        bytes.len(),
                        e
                    )
                });
                op.end(bytes.len() as u64);
                message.tx.send(()).unwrap();
                prev_offset = message.offset;
            } else {
                // Multi-block run; aggregate into a single write
                let offset = run[0];
                let mut aggregate = AlignedVec::with_capacity(len, sector_size);
                let mut txs = Vec::with_capacity(run.len());
                for offset in run {
                    let message = sorted.remove(&offset).unwrap();
                    aggregate.extend_from_slice(&message.bytes);
                    txs.push(message.tx);
                }
                let op = OpInProgress::new(stat_values);
                nix::sys::uio::pwrite(
                    file.as_raw_fd(),
                    aggregate.as_slice(),
                    i64::try_from(offset).unwrap(),
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "pwrite(fd={} off={} len={}) failed: {}",
                        file.as_raw_fd(),
                        offset,
                        aggregate.len(),
                        e
                    )
                });
                op.end(len as u64);
                for tx in txs {
                    tx.send(()).unwrap();
                }
                prev_offset = offset;
            }

            // Receive as many messages as we can without blocking
            while let Ok(message) = rx.try_recv() {
                let offset = message.offset;
                let old = sorted.insert(offset, message);
                assert!(old.is_none(), "duplicate offset {offset}");
            }
        }
    }

    // This is a desugared `async fn` so that it can return a Future that does not capture
    // `&self` (as indicated by the absence of `+ '_`).  That way, the caller can drop the
    // associated RwLock before `await`ing.
    fn write(
        &self,
        offset: u64,
        bytes: AlignedBytes,
        io_type: DiskIoType,
    ) -> impl Future<Output = ()> {
        self.verify_aligned(offset);
        self.verify_aligned(bytes.len());

        let (tx, rx) = oneshot::channel();
        let message = WriteMessage { offset, bytes, tx };

        let txs = match io_type {
            DiskIoType::WriteDataForInsert => &self.writer_txs,
            DiskIoType::MaintenanceWrite => &self.metadata_writer_txs,
            _ => panic!("invalid {:?} for write", io_type),
        };
        // Dispatch this write to a writer thread, determined based on its offset.  The first
        // DISK_WRITE_CHUNK (default 1MB) of the disk goes to the first thread, the second chunk
        // to the second thread, and so on, wrapping back around to the first thread.  Note that
        // each block allocator slab (32MB) is mapped to multiple threads, so the work is
        // distributed to multiple threads even when it's concentrated among a small number of
        // slabs.  The CHUNK (1MB) is larger than the DISK_WRITE_MAX_AGGREGATION_SIZE (128KB) so
        // that we can find aggregations that cross MAX_AGG_SIZE boundaries (e.g. from offsets
        // 100KB to 228KB).
        let writer = usize::from64(offset / DISK_WRITE_CHUNK.as_u64() % txs.len() as u64);
        txs[writer]
            .send(message)
            .unwrap_or_else(|e| panic!("writer_txs[{}].send: {}", writer, e));
        async move { measure!().fut(rx).await.unwrap() }
    }

    fn verify_aligned<N: Num + NumCast + Copy + Debug + Display>(&self, n: N) {
        let sector_size: N = NumCast::from(self.sector_size).unwrap();
        assert_eq!(
            n % sector_size,
            N::zero(),
            "{} is not sector-aligned ({})",
            n,
            sector_size
        );
    }
}

// pread/pwrite system calls are not very efficient.  In the future, on Linux,
// we can use "glommio" to use io_uring for much lower overheads.  Or SPDK
// (which can use io_uring or nvme hardware directly).
impl BlockAccess {
    pub fn new(disks: Vec<Disk>, readonly: bool) -> Self {
        let sector_size = disks
            .iter()
            .reduce(|a, b| {
                assert_eq!(a.sector_size, b.sector_size);
                a
            })
            .unwrap()
            .sector_size;

        BlockAccess {
            sector_size,
            disks: RwLock::new(disks),
            readonly,
            timebase: Instant::now(),
        }
    }

    pub fn add_disk(&self, disk: Disk) -> Result<DiskId> {
        let mut disks = self.disks.write().unwrap();
        for existing_disk in disks.iter() {
            if disk.canonical_path == existing_disk.canonical_path {
                return Err(anyhow!(
                    "disk {:?} ({:?}) is already part of the zettacache",
                    disk.path,
                    disk.canonical_path,
                ));
            }
        }
        let id = DiskId::new(disks.len());
        disks.push(disk);
        Ok(id)
    }

    // Returns the number of bytes added to the disk.
    pub fn expand_disk(&self, disk: DiskId) -> Result<ExpandDiskResponse> {
        let disks = self.disks.read().unwrap();
        let disk = &disks[disk.index()];
        let (_, new_size) = disk_sizes(disk.file)?;
        let mut size = disk.size.lock().unwrap();
        let additional_bytes = new_size.checked_sub(*size).ok_or_else(|| {
            anyhow!(
                "{disk:?} {:?} ({:?}) size decreased from {size} to {new_size}",
                disk.path,
                disk.canonical_path,
            )
        })?;
        *size = new_size;
        Ok(ExpandDiskResponse {
            additional_bytes,
            new_size,
        })
    }

    /// Note: In the future we'll support device removal in which case the
    /// DiskId's will probably not be sequential.  By using this accessor we
    /// need not assume anything about the values inside the DiskId's.
    pub fn disks(&self) -> impl Iterator<Item = DiskId> {
        (0..self.disks.read().unwrap().len()).map(DiskId::new)
    }

    pub fn path_to_disk_id(&self, path: &Path) -> Result<DiskId> {
        let canonical_path = path.canonicalize()?;
        self.disks
            .read()
            .unwrap()
            .iter()
            .position(|disk| disk.canonical_path == canonical_path)
            .map(DiskId::new)
            .ok_or_else(|| {
                anyhow!("disk {path:?} ({canonical_path:?}) is not part of the zettacache")
            })
    }

    // Gather a list of devices for zcache list_devices command.
    pub fn list_devices(&self) -> DeviceList {
        let devices = self
            .disks
            .read()
            .unwrap()
            .iter()
            .map(|d| DeviceEntry {
                name: d.path.clone(),
                size: *d.size.lock().unwrap(),
            })
            .collect();
        DeviceList { devices }
    }

    pub fn disk_size(&self, disk: DiskId) -> u64 {
        *self.disks.read().unwrap()[disk.index()]
            .size
            .lock()
            .unwrap()
    }

    pub fn disk_extent(&self, disk: DiskId) -> Extent {
        Extent {
            location: DiskLocation::new(disk, 0),
            size: self.disk_size(disk),
        }
    }

    pub fn disk_path(&self, disk: DiskId) -> PathBuf {
        self.disks.read().unwrap()[disk.index()].path.clone()
    }

    pub fn total_capacity(&self) -> u64 {
        self.disks().map(|disk| self.disk_size(disk)).sum()
    }

    /// The extent.location.offset() and extent.size must be sector-aligned.
    /// The returned Bytes will also be sector-aligned.
    pub async fn read_raw(&self, extent: Extent, io_type: DiskIoType) -> AlignedBytes {
        self.verify_aligned(extent.location.offset());
        self.verify_aligned(extent.size);

        let disk = extent.location.disk();
        let fut = self.disks.read().unwrap()[disk.index()].read(
            extent.location.offset(),
            usize::from64(extent.size),
            io_type,
        ); // drop disks RwLock before waiting for io
        fut.await
    }

    // The location.offset() and bytes.len() must be sector-aligned.  However,
    // bytes.alignment() need not be the sector size (it will be copied if not).
    pub async fn write_raw(
        &self,
        location: DiskLocation,
        bytes: AlignedBytes,
        io_type: DiskIoType,
    ) {
        assert!(
            !self.readonly,
            "attempting zettacache write in readonly mode"
        );
        self.verify_aligned(location.offset());
        self.verify_aligned(bytes.len());
        let disk = location.disk();
        let fut = self.disks.read().unwrap()[disk.index()].write(location.offset(), bytes, io_type);
        // drop disks RwLock before waiting for io
        fut.await;
    }

    pub fn round_up_to_sector<N: Num + NumCast + Copy>(&self, n: N) -> N {
        let sector_size: N = NumCast::from(self.sector_size).unwrap();
        (n + sector_size - N::one()) / sector_size * sector_size
    }

    pub fn verify_aligned<N: Num + NumCast + Copy + Debug + Display>(&self, n: N) {
        let sector_size: N = NumCast::from(self.sector_size).unwrap();
        assert_eq!(
            n % sector_size,
            N::zero(),
            "{} is not sector-aligned ({})",
            n,
            sector_size
        );
    }

    // XXX ideally this would return a sector-aligned address, so it can be used directly for a
    // directio write
    pub fn chunk_to_raw<T: Serialize>(&self, encoding: EncodeType, struct_obj: &T) -> AlignedBytes {
        let (payload, compression) = match encoding {
            EncodeType::Json => {
                let json = serde_json::to_vec(struct_obj).unwrap();
                let mut lz4_encoder = lz4::EncoderBuilder::new()
                    .level(4)
                    .build(Vec::new())
                    .unwrap();
                lz4_encoder.write_all(&json).unwrap();
                let (payload, result) = lz4_encoder.finish();
                result.unwrap();
                (payload, CompressType::Lz4)
            }
            EncodeType::Bincode => {
                let payload =
                    with_alloctag("BlockAccess::chunk_to_raw() Bincode::serialize()", || {
                        Self::bincode_options().serialize(struct_obj).unwrap()
                    });
                (payload, CompressType::None)
            }
            EncodeType::BincodeFixint => {
                let payload =
                    with_alloctag("BlockAccess::chunk_to_raw() Bincode::serialize()", || {
                        Self::bincode_fixint_options()
                            .serialize(struct_obj)
                            .unwrap()
                    });
                // XXX It's faster to not lz4 compress this, even though
                // compression would get us around 2x (27B -> 14B for index
                // entries).  But if we were to use multiple CPU's, or be able
                // to do this incrementally in the background so that we don't
                // need the absolute maximum throughput, then we should try
                // compression again.  The decompression time would still be
                // relevant but is much faster than compression so might be
                // fine.
                (payload, CompressType::None)
            }
        };

        let header = BlockHeader {
            payload_size: payload.len(),
            encoding,
            compression,
            checksum: seahash::hash(&payload),
        };
        let header_bytes = serde_json::to_vec(&header).unwrap();

        let unrounded_len = header_bytes.len() + 1 + payload.len();
        let len = self.round_up_to_sector(unrounded_len);
        let mut buf = with_alloctag("BlockAccess::chunk_to_raw() AlignedVec", || {
            AlignedVec::with_capacity(len, self.round_up_to_sector(1))
        });
        buf.extend_from_slice(&header_bytes);
        // Encode a NUL byte after the header, so that we know where it ends.
        buf.extend_from_slice(&[0]);
        // XXX copying data around; use bincode::serialize_into() to append it into a
        // larger-than-necessary vec?
        buf.extend_from_slice(&payload);
        buf.extend_from_slice(&vec![0; len - unrounded_len]);

        buf.into()
    }

    fn bincode_options() -> impl bincode::Options {
        // Note: DefaultOptions uses varint encoding (unlike bincode::serialize())
        bincode::DefaultOptions::new()
    }

    fn bincode_fixint_options() -> impl bincode::Options {
        bincode::DefaultOptions::new().with_fixint_encoding()
    }

    pub fn chunk_from_raw_impl<T: DeserializeOwned>(buf: &[u8]) -> Result<(T, usize)> {
        /// Like slice::splitn(), with n==2.  Returns None if the predicate never matches.
        fn split2<T, F: FnMut(&T) -> bool>(slice: &[T], pred: F) -> Option<(&[T], &[T])> {
            let mut split = slice.splitn(2, pred);
            let slice1 = split.next()?;
            let slice2 = split.next()?;
            assert!(split.next().is_none());
            Some((slice1, slice2))
        }

        // Note, the NUL byte is not included in either slice
        let (header_slice, post_header_slice) = split2(buf, |&c| c == b'\0')
            .ok_or_else(|| anyhow!("nul byte not found in {}-byte buf", buf.len()))?;
        let header: BlockHeader = from_json_slice(header_slice)
            .with_context(|| format!("{}-byte BlockHeader", header_slice.len()))?;

        if header.payload_size > post_header_slice.len() {
            return Err(anyhow!(
                "BlockHeader::payload_size = {} but buffer only has {} bytes remaining",
                header.payload_size,
                post_header_slice.len(),
            ));
        }
        let (payload_slice, remainder_slice) = post_header_slice.split_at(header.payload_size);

        let actual_checksum = seahash::hash(payload_slice);
        if header.checksum != actual_checksum {
            return Err(anyhow!(
                "incorrect checksum of {} bytes: expected {:x}, got {:x}",
                header.payload_size,
                header.checksum,
                actual_checksum,
            ));
        }

        let mut serde_vec = Vec::new();
        let serde_slice = match header.compression {
            CompressType::None => payload_slice,
            CompressType::Lz4 => {
                let mut decoder = lz4::Decoder::new(payload_slice)?;
                decoder.read_to_end(&mut serde_vec)?;
                let (_, result) = decoder.finish();
                result?;
                &serde_vec
            }
        };

        let struct_obj: T = match header.encoding {
            EncodeType::Json => from_json_slice(serde_slice)?,
            EncodeType::Bincode => Self::bincode_options().deserialize(serde_slice)?,
            EncodeType::BincodeFixint => Self::bincode_fixint_options().deserialize(serde_slice)?,
        };
        Ok((struct_obj, buf.len() - remainder_slice.len()))
    }

    /// returns deserialized struct and amount of the buf that was consumed
    pub fn chunk_from_raw<T: DeserializeOwned>(&self, buf: &[u8]) -> Result<(T, usize)> {
        let (struct_obj, consumed) = BlockAccess::chunk_from_raw_impl(buf)?;
        Ok((struct_obj, self.round_up_to_sector(consumed)))
    }

    pub fn io_stats<'a>(&self, agent_id: Uuid) -> IoStatsRef<'a> {
        IoStatsRef {
            cache_runtime_id: agent_id, // used to detect agent restarts across stat snapshots
            timestamp: self.timebase.elapsed(),
            disk_stats: self
                .disks
                .read()
                .unwrap()
                .iter()
                .map(|disk| disk.io_stats)
                .collect(),
        }
    }
}

/// Get size of block device.
fn blkgetsize64(file: &File) -> Result<u64> {
    nix::ioctl_read!(ioctl_blkgetsize64, 0x12u8, 114u8, u64);
    let mut cap: u64 = 0;
    let cap_ptr = &mut cap as *mut u64;
    unsafe { ioctl_blkgetsize64(file.as_raw_fd(), cap_ptr) }?;
    Ok(cap)
}

/// Get sector size of block device.
fn blksszget(file: &File) -> Result<usize> {
    nix::ioctl_read_bad!(ioctl_blksszget, 0x1268, usize);
    let mut ssz: usize = 0;
    let ssz_ptr = &mut ssz as *mut usize;
    unsafe { ioctl_blksszget(file.as_raw_fd(), ssz_ptr) }?;
    Ok(ssz)
}

/// get (sector_size, disk_size), both in bytes
fn disk_sizes(file: &File) -> Result<(usize, u64)> {
    let stat = nix::sys::stat::fstat(file.as_raw_fd())?;
    trace!("stat: {:?}", stat);
    let mode = SFlag::from_bits_truncate(stat.st_mode);
    if mode.contains(SFlag::S_IFBLK) {
        Ok((blksszget(file)?, blkgetsize64(file)?))
    } else if mode.contains(SFlag::S_IFREG) {
        Ok((*MIN_SECTOR_SIZE, u64::try_from(stat.st_size)?))
    } else {
        panic!("{file:?}: invalid file type {mode:?}");
    }
}

/// use pread() to read into an aligned vector
fn pread_aligned(file: &File, offset: i64, len: usize, alignment: usize) -> Result<AlignedVec> {
    let mut vec = with_alloctag("pread()", || AlignedVec::with_capacity(len, alignment));
    let fd = file.as_raw_fd();
    // By using the unsafe libc::pread() instead of nix::sys::uio::pread(), we
    // avoid the cost of zeroing out the vec's buffer.  pread() will initialize
    // up to `len` bytes, which the vec has capacity for.
    unsafe {
        let res = libc::pread(fd, vec.as_mut_ptr() as *mut c_void, len, offset);
        let num_bytes_read = usize::try_from(Errno::result(res)?).unwrap();
        vec.set_len(num_bytes_read);
    };
    Ok(vec)
}
