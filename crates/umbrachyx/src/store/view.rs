//! A view: every branch as one moment left it, for reading on other threads
//! while the store goes on writing.
//!
//! Taking one copies each branch's name and the root of its tree, nothing
//! more: trees share their nodes and never change once built.

use std::collections::BTreeMap;

use super::decode_cell;
use crate::tree::{Bytes, Tree};

#[derive(Debug, Clone)]
pub struct View {
    states: BTreeMap<String, Tree>,
}

impl View {
    pub(super) fn new(states: BTreeMap<String, Tree>) -> Self {
        Self { states }
    }

    pub fn has_branch(&self, branch: &str) -> bool {
        self.states.contains_key(branch)
    }

    /// The live value of `key` on `branch` at this moment.
    pub fn get(&self, branch: &str, key: &[u8]) -> Option<Bytes> {
        decode_cell(self.states.get(branch)?.get(key)?).value
    }
}
