use crate::base_types::DiskId;
use crate::base_types::DiskLocation;
use crate::base_types::Extent;
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
use std::fmt::Debug;
use std::fmt::Display;
use std::io::Read;
use std::io::Write;
use std::os::unix::prelude::AsRawFd;
use std::os::unix::prelude::OpenOptionsExt;
use std::time::Instant;
use tokio::fs::File;
use tokio::sync::Semaphore;
use util::get_tunable;
use util::AlignedBytes;
use util::AlignedVec;
use util::From64;

lazy_static! {
    static ref MIN_SECTOR_SIZE: usize = get_tunable("min_sector_size", 512);
    pub static ref DISK_WRITE_MAX_QUEUE_DEPTH: usize =
        get_tunable("disk_write_max_queue_depth", 32);
    static ref DISK_READ_MAX_QUEUE_DEPTH: usize = get_tunable("disk_read_max_queue_depth", 64);
}

#[derive(Serialize, Deserialize, Debug)]
struct BlockHeader {
    payload_size: usize,
    encoding: EncodeType,
    compression: CompressType,
    checksum: u64,
}

#[derive(Debug)]
pub struct BlockAccess {
    sector_size: usize,
    disks: Vec<Disk>,
    readonly: bool,
}

#[derive(Debug)]
pub struct Disk {
    file: File,
    size: u64,
    sector_size: usize,
    outstanding_reads: Semaphore,
    outstanding_writes: Semaphore,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum CompressType {
    None,
    Lz4,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum EncodeType {
    Json,
    Bincode,
}

// Generate ioctl function
nix::ioctl_read!(ioctl_blkgetsize64, 0x12u8, 114u8, u64);
nix::ioctl_read_bad!(ioctl_blksszget, 0x1268, usize);

#[cfg(target_os = "linux")]
const CUSTOM_OFLAGS: i32 = libc::O_DIRECT;
#[cfg(not(target_os = "linux"))]
const CUSTOM_OFLAGS: i32 = 0;

impl Disk {
    pub fn new(disk_path: &str, readonly: bool) -> Disk {
        // Note: using std file open so that this func can be non-async.
        // Although this is blocking from a tokio thread, it's used
        // infrequently, and we're already blocking from the ioctls below.
        let file = tokio::fs::File::from_std(
            std::fs::OpenOptions::new()
                .read(true)
                .write(!readonly)
                .custom_flags(CUSTOM_OFLAGS)
                .open(disk_path)
                .with_context(|| format!("opening disk '{}'", disk_path))
                .unwrap(),
        );
        let stat = nix::sys::stat::fstat(file.as_raw_fd()).unwrap();
        trace!("stat: {:?}", stat);
        let mode = SFlag::from_bits_truncate(stat.st_mode);
        let sector_size;
        let size;
        if mode.contains(SFlag::S_IFBLK) {
            size = unsafe {
                let mut cap: u64 = 0;
                let cap_ptr = &mut cap as *mut u64;
                ioctl_blkgetsize64(file.as_raw_fd(), cap_ptr).unwrap();
                cap
            };
            sector_size = unsafe {
                let mut ssz: usize = 0;
                let ssz_ptr = &mut ssz as *mut usize;
                ioctl_blksszget(file.as_raw_fd(), ssz_ptr).unwrap();
                ssz
            };
        } else if mode.contains(SFlag::S_IFREG) {
            size = u64::try_from(stat.st_size).unwrap();
            sector_size = *MIN_SECTOR_SIZE;
        } else {
            panic!("{}: invalid file type {:?}", disk_path, mode);
        }
        let this = Disk {
            file,
            size,
            sector_size,
            outstanding_reads: Semaphore::new(*DISK_READ_MAX_QUEUE_DEPTH),
            outstanding_writes: Semaphore::new(*DISK_WRITE_MAX_QUEUE_DEPTH),
        };
        info!("opening cache file {}: {:?}", disk_path, this);

        this
    }
}

// XXX this is very thread intensive.  On Linux, we can use "glommio" to use
// io_uring for much lower overheads.  Or SPDK (which can use io_uring or nvme
// hardware directly).
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
        }
    }

    /// Note: In the future we'll support device removal in which case the
    /// DiskId's will probably not be sequential.  By using this accessor we
    /// need not assume anything about the values inside the DiskId's.
    pub fn disks(&self) -> impl Iterator<Item = DiskId> {
        (0..u16::try_from(self.disks.len()).unwrap()).map(DiskId)
    }

    fn disk(&self, disk: DiskId) -> &Disk {
        &self.disks[disk.0 as usize]
    }

    pub fn disk_size(&self, disk: DiskId) -> u64 {
        self.disk(disk).size
    }

    pub fn total_capacity(&self) -> u64 {
        self.disks().map(|disk| self.disk_size(disk)).sum()
    }

    // offset and length must be sector-aligned
    pub async fn read_raw(&self, extent: Extent) -> AlignedBytes {
        self.verify_aligned(extent.location.offset);
        self.verify_aligned(extent.size);
        let disk = self.disk(extent.location.disk);
        let fd = disk.file.as_raw_fd();
        let sector_size = self.sector_size;
        let begin = Instant::now();
        let _permit = disk.outstanding_reads.acquire().await.unwrap();
        let bytes = tokio::task::spawn_blocking(move || {
            let mut v = AlignedVec::with_capacity(usize::from64(extent.size), sector_size);
            // By using the unsafe libc::pread() instead of
            // nix::sys::uio::pread(), we avoid the cost of zeroing out the
            // vec's buffer.
            unsafe {
                let res = libc::pread(
                    fd,
                    v.as_mut_ptr() as *mut c_void,
                    extent.size.try_into().unwrap(),
                    extent.location.offset.try_into().unwrap(),
                );
                let num_bytes_read = usize::try_from(Errno::result(res).unwrap()).unwrap();
                v.set_len(num_bytes_read);
            };
            assert_eq!(v.len() as u64, extent.size);
            v.into()
        })
        .await
        .unwrap();
        trace!(
            "read({:?}) returned in {}us",
            extent,
            begin.elapsed().as_micros()
        );
        bytes
    }

    // location.offset and bytes.len() must be sector-aligned.  However,
    // bytes.alignment() need not be the sector size (it will be copied if not).
    pub async fn write_raw(&self, location: DiskLocation, mut bytes: AlignedBytes) {
        assert!(
            !self.readonly,
            "attempting zettacache write in readonly mode"
        );
        let disk = self.disk(location.disk);
        let fd = disk.file.as_raw_fd();
        let length = bytes.len();
        let offset = location.offset;
        let alignment = bytes.alignment();
        self.verify_aligned(offset);
        self.verify_aligned(length);

        // directio requires the pointer to be sector-aligned
        if alignment != self.round_up_to_sector(alignment) {
            // XXX copying, this happens for AlignedBytes created from a plain Bytes
            bytes = AlignedBytes::copy_from_slice(&bytes, self.sector_size)
        }
        assert_eq!(bytes.as_ptr() as usize % self.sector_size, 0);
        let begin = Instant::now();
        let _permit = disk.outstanding_writes.acquire().await.unwrap();
        tokio::task::spawn_blocking(move || {
            nix::sys::uio::pwrite(fd, &bytes, i64::try_from(offset).unwrap()).unwrap();
            trace!(
                "write({:?} len={}) returned in {}us",
                location,
                length,
                begin.elapsed().as_micros()
            );
        })
        .await
        .unwrap();
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

    // XXX ideally this would return a sector-aligned address, so it can be used directly for a directio write
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
                let payload = Self::bincode_options().serialize(struct_obj).unwrap();
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
        let mut buf = AlignedVec::with_capacity(len, self.round_up_to_sector(1));
        buf.extend_from_slice(&header_bytes);
        // Encode a NUL byte after the header, so that we know where it ends.
        buf.extend_from_slice(&[0]);
        // XXX copying data around; use bincode::serialize_into() to append it into a larger-than-necessary vec?
        buf.extend_from_slice(&payload);
        buf.extend_from_slice(&vec![0; len - unrounded_len]);

        buf.into()
    }

    fn bincode_options() -> impl bincode::Options {
        // Note: DefaultOptions uses varint encoding (unlike bincode::serialize())
        bincode::DefaultOptions::new()
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
        };
        Ok((
            struct_obj,
            self.round_up_to_sector(header_size + data.len()),
        ))
    }
}
