use safer_ffi::prelude::*;
use std::{mem::size_of, ptr, slice};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

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
