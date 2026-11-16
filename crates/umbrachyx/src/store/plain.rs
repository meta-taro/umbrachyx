//! SPIKE: the current values of keys without history, overwritten in
//! place. One map for every branch, shared by the thread that writes and
//! the threads that read; a read holds a shard's lock for one lookup.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{PoisonError, RwLock};

use crate::tree::Bytes;

const SHARDS: usize = 64;

#[derive(Debug)]
pub struct PlainMap {
    hasher: RandomState,
    shards: Vec<RwLock<HashMap<Bytes, Bytes>>>,
}

impl PlainMap {
    pub fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            shards: (0..SHARDS).map(|_| RwLock::new(HashMap::new())).collect(),
        }
    }

    fn shard(&self, key: &[u8]) -> &RwLock<HashMap<Bytes, Bytes>> {
        let at = usize::try_from(self.hasher.hash_one(key) % SHARDS as u64).unwrap_or(0);
        &self.shards[at]
    }

    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        self.shard(key)
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    pub fn insert(&self, key: &[u8], cell: Bytes) {
        let mut shard = self
            .shard(key)
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        match shard.get_mut(key) {
            Some(held) => *held = cell,
            None => {
                shard.insert(Bytes::from(key), cell);
            }
        }
    }

    pub fn remove(&self, key: &[u8]) {
        self.shard(key)
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}
