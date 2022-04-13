use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::Instant;

use log::*;
use util::nice_p2size;
use util::VecMap;

use super::BitmapSlab;
use super::Slab;
use super::SlabId;
use crate::block_access::BlockAccess;
use crate::block_allocator::EvacuatingSlab;
use crate::block_allocator::ExtentSlab;
use crate::block_allocator::SlabPhysType;
use crate::slab_allocator::SlabAllocatorBuilder;
use crate::space_map::SpaceMapEntry;
use crate::space_map::SpaceMapPhys;

pub(super) struct Slabs(VecMap<SlabId, Slab>);

impl Slabs {
    pub fn exists(&self, id: SlabId) -> bool {
        self.0.get(id).is_some()
    }

    /// Panics if not present
    pub fn get(&self, id: SlabId) -> &Slab {
        let slab = self.0.get(id).unwrap();
        assert_eq!(slab.id, id);
        slab
    }

    /// Panics if not present
    pub fn get_mut(&mut self, id: SlabId) -> &mut Slab {
        let slab = self.0.get_mut(id).unwrap();
        assert_eq!(slab.id, id);
        slab
    }

    /// Returns old slab (or None if not present)
    pub fn insert(&mut self, id: SlabId, slab: Slab) -> Option<Slab> {
        assert_eq!(slab.id, id);
        self.0.insert(id, slab)
    }

    /// Returns old value (or None if not present)
    pub fn remove(&mut self, id: SlabId) -> Option<Slab> {
        self.0.remove(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Slab> {
        self.0.values()
    }

    pub fn range_mut<R: RangeBounds<SlabId>>(
        &mut self,
        range: R,
    ) -> impl Iterator<Item = &mut Slab> {
        self.0.range_mut(range)
    }

    pub fn total_segments(&self) -> u64 {
        self.0.values().map(|slab| slab.num_segments()).sum()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub async fn open(
        block_access: Arc<BlockAccess>,
        slab_builder: &mut SlabAllocatorBuilder,
        spacemap: &SpaceMapPhys,
        spacemap_next: &SpaceMapPhys,
    ) -> Self {
        let begin = Instant::now();
        let mut slabs = Slabs(Default::default());
        let slab_init_duration = begin.elapsed();
        info!(
            "initialized slab array in {}ms",
            slab_init_duration.as_millis()
        );

        let mut import_cb = |entry| match entry {
            SpaceMapEntry::Alloc(extent) => {
                let slab_id = slab_builder.extent_to_slab_id(extent);
                slabs.get_mut(slab_id).import_alloc(extent)
            }
            SpaceMapEntry::Free(extent) => {
                let slab_id = slab_builder.extent_to_slab_id(extent);
                slabs.get_mut(slab_id).import_free(extent)
            }
            SpaceMapEntry::SlabInfo(info) => {
                let slab_extent = slab_builder.slab_id_to_extent(info.slab_id);
                match info.slab_type {
                    SlabPhysType::BitmapBased { block_size } => {
                        slabs.insert(
                            info.slab_id,
                            BitmapSlab::new_slab(info.slab_id, slab_extent, block_size),
                        );
                    }
                    SlabPhysType::ExtentBased { max_size } => {
                        slabs.insert(
                            info.slab_id,
                            ExtentSlab::new_slab(info.slab_id, slab_extent, max_size),
                        );
                    }
                    SlabPhysType::Free => {
                        let old = slabs.remove(info.slab_id);
                        assert!(old.is_some());
                    }
                    SlabPhysType::Evacuating => {
                        slabs.insert(
                            info.slab_id,
                            EvacuatingSlab::new_slab(info.slab_id, slab_extent),
                        );
                    }
                }
            }
        };
        spacemap
            .load(block_access.clone(), slab_builder.access(), &mut import_cb)
            .await;
        spacemap_next
            .load(block_access.clone(), slab_builder.access(), &mut import_cb)
            .await;

        for slab in slabs.iter() {
            slab_builder.claim(slab.id);
        }

        info!(
            "read {} of spacemaps and processed {} entries in {}ms",
            nice_p2size(spacemap.bytes() + spacemap_next.bytes()),
            spacemap.total_entries() + spacemap_next.total_entries(),
            (begin.elapsed() - slab_init_duration).as_millis(),
        );
        slabs
    }
}
