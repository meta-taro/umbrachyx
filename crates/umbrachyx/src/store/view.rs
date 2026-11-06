//! A view: every branch as one moment left it, for reading on other threads
//! while the store goes on writing.
//!
//! Taking one copies each branch's name and the root of its tree, nothing
//! more: trees share their nodes and never change once built.

use std::collections::BTreeMap;

use super::seal::Ring;
use super::{decode_cell, reveal};
use crate::tree::{Bytes, Tree};

#[derive(Debug, Clone)]
pub struct View {
    states: BTreeMap<String, Tree>,
    /// The store's keys when values are encrypted. Shared, not copied: a
    /// subject erased after the view was taken is unreadable through it too.
    ring: Option<Ring>,
}

impl View {
    pub(super) fn new(states: BTreeMap<String, Tree>, ring: Option<Ring>) -> Self {
        Self { states, ring }
    }

    pub fn has_branch(&self, branch: &str) -> bool {
        self.states.contains_key(branch)
    }

    /// Whether `key` holds a value on `branch`, readable or erased.
    pub fn contains(&self, branch: &str, key: &[u8]) -> bool {
        self.states
            .get(branch)
            .and_then(|tree| tree.get(key))
            .is_some_and(|bytes| decode_cell(bytes).value.is_some())
    }

    /// The keys holding a value on `branch`, readable or erased, among the
    /// `count` entries of its tree after the first `skip`, in key order;
    /// and whether the tree goes on past them. Removed keys are passed over
    /// but counted, so the same `skip` names the same place as long as no
    /// key is added or removed before it. Finding that place reads the
    /// entries before it.
    pub fn keys(&self, branch: &str, skip: usize, count: usize) -> (Vec<Bytes>, bool) {
        let Some(tree) = self.states.get(branch) else {
            return (Vec::new(), false);
        };
        let mut entries = tree.iter().skip(skip);
        let keys = entries
            .by_ref()
            .take(count)
            .filter(|(_, bytes)| decode_cell(bytes).value.is_some())
            .map(|(key, _)| key.clone())
            .collect();
        (keys, entries.next().is_some())
    }

    /// The live value of `key` on `branch` at this moment.
    pub fn get(&self, branch: &str, key: &[u8]) -> Option<Bytes> {
        let value = decode_cell(self.states.get(branch)?.get(key)?).value;
        reveal(self.ring.as_ref(), key, value)
    }
}
