use std::fmt::Debug;
use std::mem::size_of;
use std::path::PathBuf;
use std::ptr;
use std::slice;

use safer_ffi::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use tokio::io;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Eq, PartialEq, Hash)]
#[derive_ReprC]
#[repr(u32)]
pub enum MessageType {
    NvList,
    ReadBlock,
    WriteBlock,
}

#[derive(Debug)]
#[derive_ReprC]
#[repr(C)]
pub struct MessageHeader {
    pub message_type: MessageType,
    pub struct_len: u32,
    pub payload_len: u32,
}

impl MessageHeader {
    pub fn new_nvlist(nvlist_len: usize) -> Self {
        MessageHeader {
            message_type: MessageType::NvList,
            struct_len: 0,
            payload_len: nvlist_len.try_into().unwrap(),
        }
    }

    pub async fn read<R: AsyncReadExt + Unpin>(stream: &mut R) -> io::Result<Self> {
        let mut header_array: [u8; size_of::<MessageHeader>()] = [0; size_of::<MessageHeader>()];
        stream.read_exact(&mut header_array).await?;
        Ok(slice_to_struct(&header_array))
    }

    pub async fn write<W: AsyncWriteExt + Unpin>(&self, stream: &mut W) -> io::Result<()> {
        stream.write_all(struct_to_slice(self)).await
    }
}

#[derive(Debug, Clone, Copy)]
#[derive_ReprC]
#[repr(C)]
pub struct ReadBlockRequest {
    pub block: u64,
    pub token: u64,
    pub heal: u32,
    pub size: u32,
}
impl ReadBlockRequest {
    // only the low bit is relevant; higher bits reserved for future use
    pub fn heal(&self) -> bool {
        self.heal & 1 == 1
    }
}

#[derive(Debug, Clone, Copy)]
#[derive_ReprC]
#[repr(C)]
pub struct ReadBlockResponse {
    pub block: u64,
    pub token: u64,
}

#[derive(Debug, Clone, Copy)]
#[derive_ReprC]
#[repr(C)]
pub struct WriteBlockRequest {
    pub block: u64,
    pub token: u64,
}

#[derive(Debug, Clone, Copy)]
#[derive_ReprC]
#[repr(C)]
pub struct WriteBlockResponse {
    pub block: u64,
    pub token: u64,
}

#[repr(C)]
union MessageStructs {
    read_block_request: ReadBlockRequest,
    read_block_response: ReadBlockResponse,
    write_block_request: WriteBlockRequest,
    write_block_response: WriteBlockResponse,
}

pub const MAX_STRUCT_LEN: usize = size_of::<MessageStructs>();

pub fn struct_to_slice<T: ReprC>(struct_ref: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((struct_ref as *const T) as *const u8, size_of::<T>()) }
}

pub fn slice_to_struct<T: ReprC>(slice: &[u8]) -> T {
    assert_eq!(slice.len(), size_of::<T>());
    unsafe { ptr::read_unaligned(slice.as_ptr() as *const _) }
}

// object agent connection API request/response types
pub const AGENT_REQUEST_TYPE: &str = "request_type";
pub const AGENT_RESPONSE_TYPE: &str = "response_type";

// kernel/agent message types
pub const TYPE_CREATE_POOL: &str = "create pool";
pub const TYPE_OPEN_POOL: &str = "open pool";
pub const TYPE_CLOSE_POOL: &str = "close pool";
pub const TYPE_GET_POOLS: &str = "get pools";
pub const TYPE_GET_DESTROYING_POOLS: &str = "get destroying pools";
pub const TYPE_CLEAR_DESTROYED_POOLS: &str = "clear destroyed pools";
pub const TYPE_RESUME_DESTROY_POOL: &str = "resume destroy pool";
pub const TYPE_BEGIN_TXG: &str = "begin txg";
pub const TYPE_END_TXG: &str = "end txg";
pub const TYPE_RESUME_COMPLETE: &str = "resume complete";
pub const TYPE_FLUSH_WRITES: &str = "flush writes";
pub const TYPE_FREE_BLOCKS: &str = "free blocks";
pub const TYPE_GET_STATS: &str = "get stats";
pub const TYPE_EXIT_AGENT: &str = "exit agent";
pub const TYPE_ENABLE_FEATURE: &str = "enable feature";
pub const TYPE_VERSION: &str = "get version";

// zcache/agent message types
pub const TYPE_CLEAR_HIT_DATA: &str = "clear hit data";
pub const TYPE_REPORT_HITS: &str = "report hits";
pub const TYPE_LIST_DEVICES: &str = "list devices";
pub const TYPE_ZCACHE_IOSTAT: &str = "zcache iostat";
pub const TYPE_ZCACHE_STATS: &str = "zcache stats";
pub const TYPE_ZCACHE_STATUS: &str = "zcache status";
pub const TYPE_ADD_DISK: &str = "add disk";
pub const TYPE_EXPAND_DISK: &str = "expand disk";
pub const TYPE_SYNC_CHECKPOINT: &str = "sync checkpoint";
pub const TYPE_INITIATE_MERGE: &str = "initiate merge";

#[derive(Serialize, Deserialize, Debug)]
pub struct AddDiskRequest {
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExpandDiskRequest {
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExpandDiskResponse {
    pub new_size: u64,
    pub additional_bytes: u64,
}

// We assume that a single write of this size is atomic.
pub const SUPERBLOCK_SIZE: usize = 4 * 1024;
