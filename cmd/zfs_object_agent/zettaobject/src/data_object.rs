use core::slice;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::iter;
use std::mem::size_of;
use std::sync::Arc;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use bytes::Bytes;
use bytesize::ByteSize;
use futures::future;
use futures::stream;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use log::*;
use more_asserts::*;
use rusoto_core::ByteStream;
use serde::Deserialize;
use serde::Serialize;
use util::async_cache::AsyncCache;
use util::async_cache::GetMethod;
use util::lazy_static_ptr;
use util::measure;
use util::tunable;
use util::with_alloctag;
use util::From64;
use zettacache::base_types::*;

use crate::access_stats::ObjectAccessOpType;
use crate::base_types::*;
use crate::object_access::ObjectAccess;

// This is part of the on-disk format.
pub const NUM_DATA_PREFIXES: u64 = 64;

tunable! {
    static ref DATA_OBJ_RANGED_GET: bool = false;
    static ref DATA_OBJ_TRY_HEADER_SIZE: ByteSize = ByteSize::kib(16);
    static ref OBJECT_CACHE_SIZE: usize = 100;

    // Number of block IDs to scan in parallel for recovery phase when the agent crashes in the
    // middle of a TXG.
    static ref RECOVERY_SCAN_COUNT: usize = 500;
}

lazy_static_ptr! {
    static ref CACHE: AsyncCache<Key, Arc<DataObject>> = AsyncCache::new(*OBJECT_CACHE_SIZE);
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Key {
    guid: PoolGuid,
    object: ObjectId,
}

impl Key {
    fn new(guid: PoolGuid, object: ObjectId) -> Self {
        Self { guid, object }
    }
}

impl Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "zfs/{}/data/{:03}/{}",
            self.guid,
            self.object.prefix(),
            self.object
        )
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
pub struct DataObjectHeader {
    pub guid: PoolGuid,      // redundant with key, for verification
    pub object: ObjectId,    // redundant with key, for verification
    pub blocks_size: u32,    // sum of blocks.values().len()
    pub next_block: BlockId, // exclusive (all blocks are < next_block)

    // Note: if this object was rewritten to consolidate adjacent objects, the
    // blocks in this object may have been originally written over a range of
    // TXG's.
    pub min_txg: Txg,
    pub max_txg: Txg, // inclusive
}

/// This is encoded on-disk in the object.  We use serde_bytes::Bytes rather
/// than Vec<u64> for the blockids (and Vec<u32> for the offsets) because
/// deserializing this needs to be very fast, and the Bytes can be deserialized
/// in constant time (it just points into the whole-object buffer), whereas
/// deserializing a Vec<_> takes O(N) with a large constant factor.
/// Deserializing needs to be fast because we do it on every "read block"
/// request, even if the object was present in the object cache (in RAM) and
/// therefore not read from S3.
///
/// The BlockIds are sorted.
#[derive(Serialize, Deserialize, Debug)]
struct DataObjectPhys<'a> {
    header: DataObjectHeader,
    #[serde(borrow)]
    blockids_raw: &'a serde_bytes::Bytes,
    #[serde(borrow)]
    offsets_raw: &'a serde_bytes::Bytes,
}

/// Alternative type for DataObjectPhys.  This is computed in constant time by
/// pointer casting.  Note it would be unsafe to cast to `&[u64]` due to byte
/// order and memory alignment constraints.  This just casts the byte slice to a
/// slice of fixed-size arrays, which makes it easier to use `from_le_bytes()`
/// to convert to u64.
struct DataObjectArrays<'a> {
    blockids: &'a [[u8; 8]],
    offsets: &'a [[u8; 4]],
}

impl<'a> DataObjectArrays<'a> {
    fn new(phys: &'a DataObjectPhys<'a>) -> Self {
        assert_eq!(phys.blockids_raw.len() % 8, 0);
        assert_eq!(phys.offsets_raw.len() % 4, 0);
        let arrays = Self {
            // It would be possible to treat the Bytes as an array of u64's
            // without any unsafe code, but accessing it as a slice of 8-byte
            // arrays is very convenient.  In particular, it lets us use
            // binary_search_by_key(), which looks for an element of the slice,
            // and we want to find a specific 8-byte array, not a specific byte.
            // It also makes the accessors (e.g. blockid()) simpler, since they
            // don't need to divide by 8, create an 8-byte slice, unwrap to an
            // 8-byte array, etc.
            blockids: unsafe {
                slice::from_raw_parts(
                    phys.blockids_raw.as_ptr() as *const [u8; 8],
                    phys.blockids_raw.len() / 8,
                )
            },
            offsets: unsafe {
                slice::from_raw_parts(
                    phys.offsets_raw.as_ptr() as *const [u8; 4],
                    phys.offsets_raw.len() / 4,
                )
            },
        };
        assert_eq!(arrays.blockids.len(), arrays.offsets.len());
        arrays
    }
    fn blockid(&self, index: usize) -> BlockId {
        BlockId(u64::from_le_bytes(self.blockids[index]))
    }
    fn offset(&self, index: usize) -> usize {
        u32::from_le_bytes(self.offsets[index]) as usize
    }
    fn len(&self) -> usize {
        self.blockids.len()
    }
    fn iter(&self) -> impl Iterator<Item = (BlockId, usize)> + '_ {
        (0..self.len()).map(|index| (self.blockid(index), self.offset(index)))
    }
    fn binary_search(&self, key: BlockId) -> Result<usize, usize> {
        self.blockids
            .binary_search_by_key(&key, |array| BlockId(u64::from_le_bytes(*array)))
    }
}

#[derive(Debug)]
pub struct DataObject {
    pub header: DataObjectHeader,
    pub blocks: HashMap<BlockId, Bytes>,
}

impl DataObject {
    /// The data object header is constrained to be less than 1MB, so that the high
    /// 44 bits are reserved for future use.
    const MAX_HEADER_LEN: usize = (1 << 20) - 1;

    pub fn key(guid: PoolGuid, object: ObjectId) -> String {
        Key::new(guid, object).to_string()
    }

    pub fn prefixes(guid: PoolGuid) -> impl Iterator<Item = String> {
        (0..NUM_DATA_PREFIXES).map(move |x| format!("zfs/{}/data/{:03}/", guid, x))
    }

    pub fn new(guid: PoolGuid, object: ObjectId, next_block: BlockId, txg: Txg) -> Self {
        assert_eq!(object.as_min_block(), next_block);
        DataObject {
            header: DataObjectHeader {
                guid,
                object,
                next_block,
                min_txg: txg,
                max_txg: txg,
                blocks_size: 0,
            },
            blocks: Default::default(),
        }
    }

    /// Returns (phys, offset_of_data)
    fn deserialize_header(bytes: &[u8]) -> Result<(DataObjectPhys<'_>, usize)> {
        let (header_len_slice, remainder_slice) = bytes.split_at(8);
        let header_len = usize::from64(u64::from_le_bytes(header_len_slice.try_into().unwrap()));
        if remainder_slice.len() < header_len {
            return Err(anyhow!(
                "header len {} greater than retrieved bytes {}",
                8 + header_len,
                bytes.len()
            ));
        }
        let (header_slice, _data_slice) = remainder_slice.split_at(header_len);
        Ok((bincode::deserialize(header_slice)?, 8 + header_len))
    }

    async fn get_impl<D: Display>(
        object_access: &ObjectAccess,
        key: D,
        stat_type: ObjectAccessOpType,
    ) -> Result<Self> {
        let bytes = object_access.get_object(key.to_string(), stat_type).await?;
        let begin = Instant::now();
        let (phys, data_offset) =
            Self::deserialize_header(&bytes).with_context(|| key.to_string())?;
        let data_bytes = bytes.slice(data_offset..);
        let arrays = DataObjectArrays::new(&phys);
        let mut blocks = with_alloctag("DataObjectPhys HashMap", || {
            HashMap::with_capacity(arrays.len())
        });
        let mut iter = arrays.iter().peekable();
        while let Some((blockid, offset)) = iter.next() {
            let next_offset = match iter.peek() {
                Some((_, next_offset)) => *next_offset,
                None => data_bytes.len(),
            };
            blocks.insert(blockid, data_bytes.slice(offset..next_offset));
        }
        let data_object = DataObject {
            header: phys.header,
            blocks,
        };

        trace!(
            "{:?}: deserialized {} blocks from {} bytes in {}us",
            phys.header.object,
            data_object.blocks.len(),
            bytes.len(),
            begin.elapsed().as_micros()
        );

        data_object.verify();
        Ok(data_object)
    }

    /// Returns (offset, next_offset), or Err if not present.
    fn locate_block(phys: DataObjectPhys, block: BlockId) -> Result<(usize, usize)> {
        let arrays = DataObjectArrays::new(&phys);
        assert_ge!(block, phys.header.object.as_min_block());
        assert_lt!(block, phys.header.next_block);
        let index = arrays
            .binary_search(block)
            .map_err(|_| anyhow!("expected {block:?} not found in {:?}", phys.header.object))?;
        let offset = arrays.offset(index);
        let next_offset = if index < arrays.len() - 1 {
            arrays.offset(index + 1)
        } else {
            assert_eq!(index, arrays.len() - 1);
            phys.header.blocks_size as usize
        };
        Ok((offset, next_offset))
    }

    /// Get this block by (typically) 2 ranged GetObject requests: one for the header and then
    /// one for the block contents.  This has higher latency but transfers less data.  This
    /// should be used if there are unlikely to be multiple get_block()'s of blocks in the same
    /// object, that could hit in the object cache.
    async fn get_block_range(
        object_access: &ObjectAccess,
        guid: PoolGuid,
        object: ObjectId,
        block: BlockId,
    ) -> Result<Bytes> {
        let key = Key::new(guid, object);
        if let Some(data) = CACHE.get_without_loading(key).await {
            return data
                .blocks
                .get(&block)
                .cloned()
                .ok_or_else(|| anyhow!("expected {block:?} not found in {object:?}"));
        }
        let header_bytes = object_access
            .get_object_range(
                key.to_string(),
                ObjectAccessOpType::ReadsGet,
                0..usize::from64(DATA_OBJ_TRY_HEADER_SIZE.as_u64()),
            )
            .await?;
        let (phys, data_offset) = Self::deserialize_header(&header_bytes)
            .with_context(|| format!("{key}: get {object:?} for {block:?}"))?;
        assert_eq!(phys.header.guid, guid);
        assert_eq!(phys.header.object, object);
        let (offset, next_offset) = Self::locate_block(phys, block)?;
        // XXX If the header_bytes already contains the data we're looking for,
        // just use the existing buffer rather than going back to S3.
        let data_bytes = object_access
            .get_object_range(
                key.to_string(),
                ObjectAccessOpType::ReadsGet,
                data_offset + offset..data_offset + next_offset,
            )
            .await?;
        assert_eq!(data_bytes.len(), next_offset - offset);
        Ok(data_bytes)
    }

    pub async fn get_from_key(
        object_access: &ObjectAccess,
        key: String,
        stat_type: ObjectAccessOpType,
    ) -> Result<Self> {
        Self::get_impl(object_access, key, stat_type).await
    }

    pub async fn get_uncached(
        object_access: &ObjectAccess,
        guid: PoolGuid,
        object: ObjectId,
        stat_type: ObjectAccessOpType,
    ) -> Result<Self> {
        // We use get_impl() rather than get_from_key() to avoid allocating and
        // copying an additional String for the key in the common case.
        Self::get_impl(object_access, Key::new(guid, object), stat_type).await
    }

    /// Note: always uses ObjectAccessOpType::ReadsGet
    pub async fn get(
        object_access: &ObjectAccess,
        guid: PoolGuid,
        object: ObjectId,
    ) -> Result<(Arc<Self>, GetMethod)> {
        measure!()
            .fut(
                CACHE.get_method(Key::new(guid, object), move |cache_key: Key| {
                    measure!().fut(
                        DataObject::get_impl(
                            object_access,
                            cache_key,
                            ObjectAccessOpType::ReadsGet,
                        )
                        .map(|x| x.map(Arc::new)),
                    )
                }),
            )
            .await
    }

    /// Note: always uses ObjectAccessOpType::ReadsGet
    pub async fn get_block(
        object_access: &ObjectAccess,
        guid: PoolGuid,
        object: ObjectId,
        block: BlockId,
    ) -> Result<Bytes> {
        if *DATA_OBJ_RANGED_GET {
            match measure!()
                .fut(Self::get_block_range(object_access, guid, object, block))
                .await
            {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    // presumably we didn't get enough bytes for the header; get the whole object
                    debug!("error deserializing {}: {}", object, e);
                    measure!("DataObject header deserialize failure").hit();
                }
            }
        }
        // Get this block by reading the whole object, via the object cache.  This should be used
        // if there are likely to be subsequent get_block() calls on other blocks in the same
        // object.
        Self::get(object_access, guid, object)
            .await?
            .0
            .blocks
            .get(&block)
            .cloned()
            .ok_or_else(|| anyhow!("expected {:?} not found in {:?}", block, object))
    }

    /// If this object is already in the cache, or a GetObject is in progress for it, and the
    /// block is present in the object, then return it.  Otherwise, return None rather than
    /// initiating a GetObject for it.
    pub async fn peek_block(guid: PoolGuid, object: ObjectId, block: BlockId) -> Option<Bytes> {
        CACHE
            .get_without_loading(Key::new(guid, object))
            .await
            .and_then(|data| data.blocks.get(&block).cloned())
    }

    fn invalidate_cache(guid: PoolGuid, object: ObjectId) {
        CACHE.invalidate(Key::new(guid, object));
    }

    pub async fn put(&self, object_access: &ObjectAccess, stat_type: ObjectAccessOpType) {
        let begin = Instant::now();

        let mut offset = 0;
        let mut blockids_raw = Vec::with_capacity(self.blocks.len() * size_of::<u64>());
        let mut offsets_raw = Vec::with_capacity(self.blocks.len() * size_of::<u32>());
        let mut sorted = self.blocks.keys().cloned().collect::<Vec<_>>();
        sorted.sort_unstable();
        for blockid in &sorted {
            let bytes = self.blocks.get(blockid).unwrap();
            // Use fully-qualified function invocation `u64::to_le_bytes(x)`
            // rather than method invocation `x.to_le_bytes()` so that if the
            // type of `blockid.0` changes, this will fail to compile, rather
            // than silently changing the on-disk format.
            blockids_raw.extend_from_slice(&u64::to_le_bytes(blockid.0));
            offsets_raw.extend_from_slice(&u32::to_le_bytes(offset));
            offset += u32::try_from(bytes.len()).unwrap();
        }
        let data_len = offset as usize;

        let phys = DataObjectPhys {
            header: self.header,
            blockids_raw: serde_bytes::Bytes::new(&blockids_raw),
            offsets_raw: serde_bytes::Bytes::new(&offsets_raw),
        };

        let phys_bytes = Bytes::from(with_alloctag(
            "DataObject::put() bincode::serialize()",
            || bincode::serialize(&phys).unwrap(),
        ));
        trace!(
            "{:?}: serialized {} blocks in {} bytes in {}us",
            self.header.object,
            self.blocks.len(),
            phys_bytes.len(),
            begin.elapsed().as_micros()
        );
        assert_le!(phys_bytes.len(), Self::MAX_HEADER_LEN);
        self.verify();
        let header_len_bytes = Bytes::copy_from_slice(&u64::to_le_bytes(phys_bytes.len() as u64));
        let len = header_len_bytes.len() + phys_bytes.len() + data_len;
        object_access
            .put_object_stream(
                Self::key(self.header.guid, self.header.object),
                || {
                    // The returned ByteStream can't capture `self`.
                    #[allow(clippy::needless_collect)]
                    let my_bytesvec = sorted
                        .iter()
                        .map(|block| self.blocks.get(block).unwrap().clone())
                        .collect::<Vec<_>>();
                    // By passing a ByteStream here, we avoid copying the block contents into a
                    // contiguous buffer.
                    let iter = iter::once(header_len_bytes.clone())
                        .chain(iter::once(phys_bytes.clone()))
                        .chain(my_bytesvec.into_iter())
                        .map(Ok);
                    (ByteStream::new_with_size(stream::iter(iter), len), len)
                },
                stat_type,
            )
            .await;
        // Note that we need to PutObject before invalidating the cache.  If a get() is called
        // while put() is in progress, it may see the old or new value, which is fine.  After
        // put() returns, get() must return the new value.  If we invalidated before the
        // PutObject, a concurrent get() could retrieve the old value and add it to the cache,
        // allowing the old value to be read (from the cache) after put() returns.
        Self::invalidate_cache(self.header.guid, self.header.object);
    }

    pub fn calculate_blocks_size(&self) -> u32 {
        self.blocks
            .values()
            .map(|block| block.len())
            .sum::<usize>()
            .try_into()
            .unwrap()
    }

    fn verify(&self) {
        assert_eq!(self.header.blocks_size, self.calculate_blocks_size());
        assert_le!(self.header.min_txg, self.header.max_txg);
        assert_le!(self.header.object.as_min_block(), self.header.next_block);
        if !self.blocks.is_empty() {
            assert_le!(
                self.header.object.as_min_block(),
                self.blocks.keys().min().unwrap()
            );
            assert_gt!(self.header.next_block, self.blocks.keys().max().unwrap());
        }
    }

    pub fn block(&self, block: BlockId) -> Bytes {
        self.blocks.get(&block).unwrap().clone()
    }

    pub fn blocks_len(&self) -> u32 {
        u32::try_from(self.blocks.len()).unwrap()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub async fn next_uncached(
        object_access: &ObjectAccess,
        guid: PoolGuid,
        start_from: ObjectId,
        end_with: ObjectId,
    ) -> Option<Self> {
        stream::iter(
            (0..)
                .map(|i| ObjectId::new(start_from.as_min_block() + i))
                .take_while(|&object| object <= end_with)
                .map(|object| async move {
                    Self::get_uncached(object_access, guid, object, ObjectAccessOpType::ReadsGet)
                        .await
                }),
        )
        .buffered(*RECOVERY_SCAN_COUNT)
        .filter_map(|result| future::ready(result.ok()))
        .next()
        .await
    }

    pub fn list_all(
        object_access: &ObjectAccess,
        guid: PoolGuid,
    ) -> impl Stream<Item = ObjectId> + '_ {
        stream::select_all(Self::prefixes(guid).map(|prefix| {
            object_access
                .list_objects(prefix, false)
                .map(|str| ObjectId::from_key(&str))
        }))
    }
}

impl Display for DataObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: blocks={} bytes={} next={:?} TXG[{},{}]",
            self.header.object,
            self.blocks.len(),
            self.header.blocks_size,
            self.header.next_block,
            self.header.min_txg.0,
            self.header.max_txg.0,
        )
    }
}
