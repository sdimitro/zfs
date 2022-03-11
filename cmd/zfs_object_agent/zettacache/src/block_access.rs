use std::fmt::Debug;
use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::unix::prelude::AsRawFd;
use std::os::unix::prelude::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use bincode::Options;
use lazy_static::lazy_static;
use libc::c_void;
use log::*;
use nix::errno::Errno;
use nix::sys::stat::SFlag;
use num::Num;
use num::NumCast;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::oneshot;
use util::get_tunable;
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

lazy_static! {
    static ref MIN_SECTOR_SIZE: usize = get_tunable("min_sector_size", 512);
    static ref DISK_WRITE_MAX_QUEUE_DEPTH: usize = get_tunable("disk_write_max_queue_depth", 32);
    static ref DISK_METADATA_WRITE_MAX_QUEUE_DEPTH: usize =
        get_tunable("disk_metadata_write_max_queue_depth", 16);
    static ref DISK_READ_MAX_QUEUE_DEPTH: usize = get_tunable("disk_read_max_queue_depth", 64);
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
    disks: Vec<Disk>,
    readonly: bool,
    timebase: Instant,
}

#[derive(Debug)]
pub struct Disk {
    // We want all the reader/writer_threads to share the same file descriptor,
    // but we don't have a mechanism to ensure that they stop using the fd when
    // the DiskStruct is dropped and the fd is closed.  To solve this we simply
    // never close the fd.  The fd is owned by the File, and we leave a
    // reference to it here to indicate that it's related to this Disk, even
    // though it's only used via the reader/writer_threads.
    #[allow(dead_code)]
    file: &'static File,

    device_path: String,
    size: u64,
    sector_size: usize,
    io_stats: &'static DiskIoStats,
    reader_tx: flume::Sender<ReadMessage>,
    writer_tx: flume::Sender<WriteMessage>,
    metadata_writer_tx: flume::Sender<WriteMessage>,
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
    io_type: DiskIoType,
    tx: oneshot::Sender<()>,
}

impl Disk {
    pub fn new(disk_path: &str, readonly: bool) -> Result<Disk> {
        // Note: using std file open so that this func can be non-async.
        // Although this is blocking from a tokio thread, it's used
        // infrequently, and we're already blocking from the ioctls to get the
        // disk size and block size.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!readonly)
            .custom_flags(CUSTOM_OFLAGS)
            .open(disk_path)
            .with_context(|| format!("opening disk '{}'", disk_path))?;
        // see comment in `struct Disk`
        let file = &*Box::leak(Box::new(file));
        let stat = nix::sys::stat::fstat(file.as_raw_fd())?;
        trace!("stat: {:?}", stat);
        let mode = SFlag::from_bits_truncate(stat.st_mode);
        let sector_size;
        let size;
        if mode.contains(SFlag::S_IFBLK) {
            size = blkgetsize64(file)?;
            sector_size = blksszget(file)?;
        } else if mode.contains(SFlag::S_IFREG) {
            size = u64::try_from(stat.st_size)?;
            sector_size = *MIN_SECTOR_SIZE;
        } else {
            panic!("{}: invalid file type {:?}", disk_path, mode);
        }

        let device = Path::new(disk_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        let (reader_tx, reader_rx) = flume::unbounded();
        let (writer_tx, writer_rx) = flume::unbounded();
        let (metadata_writer_tx, metadata_writer_rx) = flume::unbounded();

        let io_stats = &*Box::leak(Box::new(DiskIoStats::new(device)));

        let this = Disk {
            file,
            device_path: disk_path.to_string(),
            size,
            sector_size,
            io_stats,
            reader_tx,
            writer_tx,
            metadata_writer_tx,
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
            for _ in 0..*DISK_WRITE_MAX_QUEUE_DEPTH {
                let rx = writer_rx.clone();
                std::thread::spawn(move || {
                    Self::writer_thread(file, io_stats, sector_size, rx);
                });
            }
            for _ in 0..*DISK_METADATA_WRITE_MAX_QUEUE_DEPTH {
                let rx = metadata_writer_rx.clone();
                std::thread::spawn(move || {
                    Self::writer_thread(file, io_stats, sector_size, rx);
                });
            }
        }
        info!("opening cache file {}: {:?}", disk_path, this);

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
            let vec = pread_aligned(
                file,
                message.offset.try_into().unwrap(),
                message.size,
                sector_size,
            )
            .unwrap();
            assert_eq!(vec.len(), message.size);
            op.end(message.size as u64);
            message.tx.send(vec.into()).unwrap();
        }
    }

    async fn read(&self, offset: u64, size: usize, io_type: DiskIoType) -> AlignedBytes {
        self.verify_aligned(offset);
        self.verify_aligned(size);

        let (tx, rx) = oneshot::channel();
        let message = ReadMessage {
            offset,
            size,
            io_type,
            tx,
        };

        self.reader_tx.send_async(message).await.unwrap();
        let bytes = rx.await.unwrap();
        bytes
    }

    fn writer_thread(
        file: &'static File,
        io_stats: &'static DiskIoStats,
        sector_size: usize,
        rx: flume::Receiver<WriteMessage>,
    ) {
        while let Ok(message) = rx.recv() {
            let offset = i64::try_from(message.offset).unwrap();
            // Directio requires the pointer to be sector-aligned.  The message
            // sender aligned it for us if necessary.
            assert_eq!(message.bytes.alignment() % sector_size, 0);
            assert_eq!(message.bytes.as_ptr() as usize % sector_size, 0);
            let op = OpInProgress::new(&io_stats.stats[message.io_type]);
            nix::sys::uio::pwrite(file.as_raw_fd(), &message.bytes, offset).unwrap();
            op.end(message.bytes.len() as u64);
            message.tx.send(()).unwrap();
        }
    }

    async fn write(&self, offset: u64, bytes: AlignedBytes, io_type: DiskIoType) {
        self.verify_aligned(offset);
        self.verify_aligned(bytes.len());
        self.verify_aligned(bytes.alignment());

        let (tx, rx) = oneshot::channel();
        let message = WriteMessage {
            offset,
            bytes,
            io_type,
            tx,
        };

        let tx = match io_type {
            DiskIoType::WriteDataForInsert => &self.writer_tx,
            DiskIoType::MaintenanceWrite => &self.metadata_writer_tx,
            _ => panic!("invalid {:?} for write", io_type),
        };
        tx.send_async(message).await.unwrap();
        rx.await.unwrap();
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
            disks,
            readonly,
            timebase: Instant::now(),
        }
    }

    /// Note: In the future we'll support device removal in which case the
    /// DiskId's will probably not be sequential.  By using this accessor we
    /// need not assume anything about the values inside the DiskId's.
    pub fn disks(&self) -> impl Iterator<Item = DiskId> {
        (0..self.disks.len()).map(DiskId::new)
    }

    // Gather a list of devices for zcache list_devices command.
    pub fn list_devices(&self) -> DeviceList {
        let devices = self
            .disks
            .iter()
            .map(|d| DeviceEntry {
                name: d.device_path.to_string(),
                size: d.size,
            })
            .collect();
        DeviceList { devices }
    }

    fn disk(&self, disk: DiskId) -> &Disk {
        &self.disks[disk.get()]
    }

    pub fn disk_size(&self, disk: DiskId) -> u64 {
        self.disk(disk).size
    }

    pub fn disk_path(&self, disk: DiskId) -> String {
        self.disk(disk).device_path.to_string()
    }

    pub fn total_capacity(&self) -> u64 {
        self.disks().map(|disk| self.disk_size(disk)).sum()
    }

    /// The extent.location.offset() and extent.size must be sector-aligned.
    /// The returned Bytes will also be sector-aligned.
    pub async fn read_raw(&self, extent: Extent, io_type: DiskIoType) -> AlignedBytes {
        self.verify_aligned(extent.location.offset());
        self.verify_aligned(extent.size);

        self.disk(extent.location.disk())
            .read(
                extent.location.offset(),
                usize::from64(extent.size),
                io_type,
            )
            .await
    }

    // The location.offset() and bytes.len() must be sector-aligned.  However,
    // bytes.alignment() need not be the sector size (it will be copied if not).
    pub async fn write_raw(
        &self,
        location: DiskLocation,
        mut bytes: AlignedBytes,
        io_type: DiskIoType,
    ) {
        assert!(
            !self.readonly,
            "attempting zettacache write in readonly mode"
        );
        self.verify_aligned(location.offset());
        self.verify_aligned(bytes.len());
        if bytes.alignment() != self.round_up_to_sector(bytes.alignment()) {
            // XXX copying, this happens for AlignedBytes created from a plain Bytes
            bytes = AlignedBytes::copy_from_slice(&bytes, self.sector_size);
        };
        self.disk(location.disk())
            .write(location.offset(), bytes, io_type)
            .await
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

    /// returns deserialized struct and amount of the buf that was consumed
    pub fn chunk_from_raw<T: DeserializeOwned>(&self, buf: &[u8]) -> Result<(T, usize)> {
        // size includes the terminating NUL byte
        let header_size = buf.iter().position(|&c| c == b'\0').unwrap() + 1;
        let header: BlockHeader = serde_json::from_slice(&buf[..header_size - 1])?;

        if header.payload_size > buf.len() - header_size {
            return Err(anyhow!(
                "invalid length {}: expected at most {} bytes",
                header.payload_size,
                buf.len() - header_size
            ));
        }

        let data = &buf[header_size..header.payload_size + header_size];
        assert_eq!(data.len(), header.payload_size);
        let actual_checksum = seahash::hash(data);
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
            CompressType::None => data,
            CompressType::Lz4 => {
                let mut decoder = lz4::Decoder::new(data).unwrap();
                decoder.read_to_end(&mut serde_vec).unwrap();
                let (_, result) = decoder.finish();
                result.unwrap();
                &serde_vec
            }
        };

        let struct_obj: T = match header.encoding {
            EncodeType::Json => serde_json::from_slice(serde_slice)?,
            EncodeType::Bincode => Self::bincode_options().deserialize(serde_slice)?,
            EncodeType::BincodeFixint => Self::bincode_fixint_options().deserialize(serde_slice)?,
        };
        Ok((
            struct_obj,
            self.round_up_to_sector(header_size + data.len()),
        ))
    }

    /// Return the I/O stats collected as a serialized json string.
    pub fn io_stats_as_json(&self, agent_id: Uuid) -> String {
        serde_json::to_string(&IoStatsRef {
            cache_runtime_id: agent_id, // used to detect agent restarts across stat snapshots
            timestamp: self.timebase.elapsed(),
            disk_stats: self.disks.iter().map(|disk| disk.io_stats).collect(),
        })
        .unwrap()
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
