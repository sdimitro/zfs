use std::sync::RwLock;

use log::*;
use serde::Deserialize;
use serde::Serialize;

use crate::base_types::PoolGuid;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct PoolId(pub u8);

pub struct PoolGuidMapping {
    guids: RwLock<Vec<PoolGuid>>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct PoolGuidMappingPhys(Vec<PoolGuid>);

impl PoolGuidMapping {
    pub fn open(phys: PoolGuidMappingPhys) -> Self {
        Self {
            guids: RwLock::new(phys.0),
        }
    }

    pub fn to_phys(&self) -> PoolGuidMappingPhys {
        PoolGuidMappingPhys(self.guids.read().unwrap().clone())
    }

    fn try_map(guids: &[PoolGuid], guid: PoolGuid) -> Option<PoolId> {
        // XXX - this is an O(n) algorithm, which is fine for a small number of pools, but we may
        // want to use a hashmap for this if there are lots of pools.
        for (id, mapped_guid) in guids.iter().enumerate() {
            if *mapped_guid == guid {
                return Some(PoolId(u8::try_from(id).unwrap()));
            }
        }
        None
    }

    /// Return the PoolId (index) associated with the PoolGuid.  If not found, the GUID is added
    /// to the known set and a new PoolId generated.
    pub fn map_pool_guid(&self, guid: PoolGuid) -> PoolId {
        // Fast path: check for existing entry, without the write lock.
        if let Some(id) = Self::try_map(&self.guids.read().unwrap(), guid) {
            return id;
        }

        // No entry found; add a new one.
        let mut guids = self.guids.write().unwrap();
        // Need to check again since we dropped the lock
        if let Some(id) = Self::try_map(&guids, guid) {
            return id;
        }
        let id = u8::try_from(guids.len()).unwrap();
        debug!("New {:?} added with {:?}", guid, PoolId(id));
        guids.push(guid);
        PoolId(id)
    }
}
