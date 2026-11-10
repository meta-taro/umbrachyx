//! A view: every branch as one moment left it, for reading on other threads
//! while the store goes on writing.
//!
//! Taking one copies each branch's name and the roots of its tree and of
//! its latest-value map, nothing more: both share their nodes and never
//! change once built.

use std::collections::BTreeMap;

use super::seal::Ring;
use super::{decode_cell, reveal};
use crate::hamt::Hamt;
use crate::tree::{Bytes, Tree};

#[derive(Debug, Clone)]
pub struct View {
    /// Each branch's tree, and the map of its latest-value tables.
    states: BTreeMap<String, (Tree, Hamt)>,
    /// The store's keys when values are encrypted. Shared, not copied: a
    /// subject erased after the view was taken is unreadable through it too.
    ring: Option<Ring>,
}

impl View {
    pub(super) fn new(states: BTreeMap<String, (Tree, Hamt)>, ring: Option<Ring>) -> Self {
        Self { states, ring }
    }

    pub fn has_branch(&self, branch: &str) -> bool {
        self.states.contains_key(branch)
    }

    /// Whether `key` holds a value on `branch`, readable or erased.
    pub fn contains(&self, branch: &str, key: &[u8]) -> bool {
        self.cell(branch, key)
            .is_some_and(|bytes| decode_cell(bytes).value.is_some())
    }

    /// A latest-value map holds only live keys, and no key is in both.
    fn cell(&self, branch: &str, key: &[u8]) -> Option<&Bytes> {
        let (tree, latest) = self.states.get(branch)?;
        tree.get(key).or_else(|| latest.get(key))
    }

    /// The keys holding a value on `branch`, readable or erased, among the
    /// `count` entries after the first `skip`; and whether more follow. The
    /// tree comes first, in key order, then the latest-value tables, in an
    /// order that means nothing and changes when the process restarts, so a
    /// place named before a restart may skip or repeat their keys. Removed keys are passed over but counted,
    /// so the same `skip` names the same place as long as no key is added
    /// or removed before it. Finding that place reads the entries before
    /// it.
    pub fn keys(&self, branch: &str, skip: usize, count: usize) -> (Vec<Bytes>, bool) {
        let Some((tree, latest)) = self.states.get(branch) else {
            return (Vec::new(), false);
        };
        let mut entries = tree.iter().chain(latest.iter()).skip(skip);
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
        let value = decode_cell(self.cell(branch, key)?).value;
        reveal(self.ring.as_ref(), key, value)
    }
}
