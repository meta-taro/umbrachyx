//! A view: every branch as one moment left it, for reading on other threads
//! while the store goes on writing.
//!
//! Taking one copies each branch's name and the roots of its tree and of
//! its latest-value map, nothing more: both share their nodes and never
//! change once built.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::plain::PlainMap;
use super::seal::Ring;
use super::{decode_cell, reveal};
use crate::hamt::Hamt;
use crate::log::Engine;
use crate::tree::{Bytes, Tree};

#[derive(Debug, Clone)]
pub struct View {
    /// Each branch's tree, and the map of its latest-value tables.
    states: BTreeMap<String, (Tree, Hamt)>,
    /// The store's keys when values are encrypted. Shared, not copied: a
    /// subject erased after the view was taken is unreadable through it too.
    ring: Option<Ring>,
    /// The values of tables that keep no history. Not a moment kept: the
    /// one map the store goes on writing, the same for every branch, so
    /// a view reads what such a table holds now.
    plain: Arc<PlainMap>,
    /// The tables that keep no history, by name.
    plain_tables: Arc<HashMap<Vec<u8>, Engine>>,
}

impl View {
    pub(super) fn new(
        states: BTreeMap<String, (Tree, Hamt)>,
        ring: Option<Ring>,
        plain: Arc<PlainMap>,
        plain_tables: Arc<HashMap<Vec<u8>, Engine>>,
    ) -> Self {
        Self {
            states,
            ring,
            plain,
            plain_tables,
        }
    }

    /// Whether `key` is in a table that keeps no history, and so is
    /// written with a put rather than a commit.
    pub fn is_plain(&self, key: &[u8]) -> bool {
        self.plain_engine_of(key).is_some()
    }

    /// `value` as the store would hold it for `key`, if a put of it
    /// through a committer is answered before it is on disk and this
    /// thread can make it ready without the store: the key's table is
    /// plain-async, and the value is sealed here if values are
    /// encrypted. `None` if the put is for the store to take whole: the
    /// table is of another kind, or the key's subject has no key the
    /// store has sealed with yet, and only the store makes keys.
    pub fn held_early(&self, key: &[u8], value: &[u8]) -> Option<Vec<u8>> {
        if self.plain_engine_of(key) != Some(Engine::PlainAsync) {
            return None;
        }
        match &self.ring {
            Some(ring) => super::seal::seal_apart(ring, key, value),
            None => Some(value.to_vec()),
        }
    }

    fn plain_engine_of(&self, key: &[u8]) -> Option<Engine> {
        if self.plain_tables.is_empty() {
            return None;
        }
        let table = super::tables::table_of(key)?;
        self.plain_tables.get(table).copied()
    }

    pub fn has_branch(&self, branch: &str) -> bool {
        self.states.contains_key(branch)
    }

    /// Whether `key` holds a value on `branch`, readable or erased.
    pub fn contains(&self, branch: &str, key: &[u8]) -> bool {
        self.cell(branch, key)
            .is_some_and(|bytes| decode_cell(bytes).value.is_some())
            || (self.has_branch(branch) && self.plain.contains(key))
    }

    /// A latest-value map holds only live keys, and no key is in both.
    fn cell(&self, branch: &str, key: &[u8]) -> Option<&[u8]> {
        let (tree, latest) = self.states.get(branch)?;
        tree.get(key)
            .or_else(|| latest.get(key).map(|cell| &**cell))
    }

    /// The keys holding a value on `branch`, readable or erased, among the
    /// `count` entries after the first `skip`; and whether more follow. The
    /// tree comes first, in key order, then the latest-value tables, then
    /// the tables that keep no history: both in an order that means
    /// nothing and changes when the process restarts, and for the last
    /// also when one of their keys is added or removed, so a place named
    /// before that may skip or repeat their keys. Removed keys of the tree
    /// are passed over but counted, so the same `skip` names the same
    /// place as long as no key is added or removed before it. Finding
    /// that place reads the tree's and the latest-value entries before it.
    pub fn keys(&self, branch: &str, skip: usize, count: usize) -> (Vec<Bytes>, bool) {
        let Some((tree, latest)) = self.states.get(branch) else {
            return (Vec::new(), false);
        };
        let latest = latest.iter().map(|(key, cell)| (key, &**cell));
        let mut entries = tree.iter().chain(latest);
        let passed = entries.by_ref().take(skip).count();
        let mut read = 0;
        let mut keys: Vec<Bytes> = entries
            .by_ref()
            .take(count)
            .inspect(|_| read += 1)
            .filter(|(_, bytes)| decode_cell(bytes).value.is_some())
            .map(|(key, _)| key.clone())
            .collect();
        if read == count {
            let has_more = entries.next().is_some() || !self.plain.is_empty();
            return (keys, has_more);
        }
        let (plain, has_more) = self.plain.keys(skip - passed, count - read);
        keys.extend(plain);
        (keys, has_more)
    }

    /// The live value of `key` on `branch` at this moment.
    pub fn get(&self, branch: &str, key: &[u8]) -> Option<Bytes> {
        let value = match self.cell(branch, key) {
            Some(cell) => decode_cell(cell).value,
            None => {
                self.states.get(branch)?;
                Some(self.plain.get(key)?)
            }
        };
        reveal(self.ring.as_ref(), key, value)
    }
}
