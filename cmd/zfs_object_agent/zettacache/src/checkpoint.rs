use std::time::Instant;

use futures::future;
use futures::stream::FuturesOrdered;
use futures::stream::StreamExt;
use log::debug;
use log::info;
use serde::Deserialize;
use serde::Serialize;
use util::nice_p2size;
use util::zettacache_stats::DiskIoType;
use util::From64;

use crate::base_types::Atime;
use crate::base_types::Extent;
use crate::block_access::BlockAccess;
use crate::block_access::EncodeType;
use crate::block_allocator::BlockAllocatorPhys;
use crate::block_based_log::BlockBasedLogPhys;
use crate::index::IndexRunPhys;
use crate::pool_id::PoolGuidMappingPhys;
use crate::size_histogram::SizeHistogramPhys;
use crate::slab_allocator::SlabAllocator;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::slab_allocator::SlabAllocatorPhys;
use crate::zettacache::merge::MergeProgressPhys;
use crate::zettacache::OperationLogEntry;

#[derive(Serialize, Deserialize, Default, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct CheckpointId(pub u64);
impl CheckpointId {
    pub fn next(&self) -> CheckpointId {
        CheckpointId(self.0 + 1)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct CheckpointPhys {
    pub id: CheckpointId,
    pub pool_guids: PoolGuidMappingPhys,
    pub block_allocator: BlockAllocatorPhys,
    pub slab_allocator: SlabAllocatorPhys,
    pub last_atime: Atime,
    pub old_index: IndexRunPhys,
    pub operation_log: BlockBasedLogPhys<OperationLogEntry>,
    pub size_histogram: SizeHistogramPhys,
    pub merge_progress: Option<MergeProgressPhys>,
}

impl CheckpointPhys {
    pub async fn read(block_access: &BlockAccess, extents: &[Extent]) -> Self {
        let raw = extents
            .iter()
            .map(|&extent| block_access.read_raw(extent, DiskIoType::MaintenanceRead))
            .collect::<FuturesOrdered<_>>()
            .fold(Vec::new(), |mut vec, bytes| {
                vec.extend_from_slice(&bytes);
                future::ready(vec)
            })
            .await;
        let (this, _): (Self, usize) = block_access.chunk_from_raw(&raw).unwrap();
        debug!("got {:#?}", this);
        this
    }

    pub async fn write(
        &self,
        block_access: &BlockAccess,
        slab_allocator: &SlabAllocator,
    ) -> Vec<Extent> {
        let begin = Instant::now();
        let raw = block_access.chunk_to_raw(EncodeType::Json, self);
        let chunk_to_raw_duration = begin.elapsed();

        let begin = Instant::now();
        let extents = raw
            .as_ref()
            .chunks(usize::from64(self.slab_allocator.slab_size()))
            .map(|chunk| {
                let raw = raw.clone();
                let extent = slab_allocator
                    .slab_id_to_extent(slab_allocator.allocate_reserved())
                    .range(0, chunk.len() as u64);
                async move {
                    block_access
                        .write_raw(
                            extent.location,
                            raw.slice_ref(chunk),
                            DiskIoType::MaintenanceWrite,
                        )
                        .await;
                    extent
                }
            })
            .collect::<FuturesOrdered<_>>()
            .collect::<Vec<_>>()
            .await;
        info!(
            "ZettaCheckpointPhys.write({:?}): {} ({} slabs), to raw in {}ms, to disk in {}ms",
            self.id,
            nice_p2size(raw.len() as u64),
            extents.len(),
            chunk_to_raw_duration.as_millis(),
            begin.elapsed().as_millis(),
        );
        extents
    }

    pub fn claim(&self, builder: &mut SlabAllocatorBuilder) {
        self.block_allocator.claim(builder);
        self.old_index.claim(builder);
        self.operation_log.claim(builder);
        if let Some(progress) = self.merge_progress.as_ref() {
            progress.claim(builder);
        }
    }
}
