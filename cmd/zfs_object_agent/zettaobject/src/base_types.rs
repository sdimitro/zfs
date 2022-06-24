use std::fmt::*;

use serde::Deserialize;
use serde::Serialize;
use zettacache::base_types::BlockId;

use crate::data_object::NUM_DATA_PREFIXES;

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct Txg(pub u64);
impl Display for Txg {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{:020}", self.0)
    }
}

impl Txg {
    pub fn checked_sub(self, rhs: u64) -> Option<Txg> {
        if self.0 < rhs {
            None
        } else {
            Some(Txg(self.0 - rhs))
        }
    }

    pub fn from_key(key: &str) -> Self {
        Txg(key.rsplit_once('/').unwrap().1.parse().unwrap())
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct ObjectId(u64);
impl Display for ObjectId {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "{:020}", self.0)
    }
}
impl ObjectId {
    pub fn new(min_block: BlockId) -> ObjectId {
        ObjectId(min_block.0)
    }

    pub fn as_min_block(self) -> BlockId {
        BlockId(self.0)
    }

    pub fn prefix(self) -> u64 {
        self.0 % NUM_DATA_PREFIXES
    }

    /// This function parses a key into an object id. It works for any key
    /// where the last path component is the object id.
    pub fn from_key(key: &str) -> Self {
        ObjectId(key.rsplit_once('/').unwrap().1.parse().unwrap())
    }
}
