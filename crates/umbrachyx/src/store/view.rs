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

    /// The live value of `key` on `branch` at this moment.
    pub fn get(&self, branch: &str, key: &[u8]) -> Option<Bytes> {
        let value = decode_cell(self.states.get(branch)?.get(key)?).value;
        reveal(self.ring.as_ref(), key, value)
    }
}
