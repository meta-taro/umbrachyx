//! Candidate structures for the tree that holds Umbrachyx's state and history.
//!
//! Each candidate is a persistent, content-addressed map. An insert returns a
//! new version and shares every untouched node with the old one; the root
//! hash names the whole content. They exist to be measured against each other
//! (`examples/tree_bench.rs`); only the one chosen moves into the database.
//!
//! Deletes are left out. They do not change what is being compared, the cost
//! of a diff, and each candidate would need its own merge-on-delete code.

use std::sync::Arc;

pub mod btree;
pub mod data;
pub mod hamt;
mod hash;
mod ordered;
pub mod prolly;

pub use hash::Digest;

/// Keys and values are shared, not copied, between versions of a tree.
pub type Bytes = Arc<[u8]>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { key: Bytes, value: Bytes },
    Removed { key: Bytes, value: Bytes },
    Modified { key: Bytes, old: Bytes, new: Bytes },
}

impl Change {
    pub fn key(&self) -> &Bytes {
        match self {
            Self::Added { key, .. } | Self::Removed { key, .. } | Self::Modified { key, .. } => key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Sorted by key.
    pub changes: Vec<Change>,
    /// Nodes read on both sides: the work the diff did, independent of the
    /// machine it ran on.
    pub nodes_visited: usize,
}

pub trait Tree: Clone {
    const NAME: &'static str;

    fn empty() -> Self;
    /// Builds a tree from entries sorted by key, with no key twice.
    fn from_sorted(entries: Vec<(Bytes, Bytes)>) -> Self;
    fn get(&self, key: &[u8]) -> Option<&Bytes>;
    /// Inserts or replaces, returning a new version. `self` is unchanged.
    fn insert(&self, key: Bytes, value: Bytes) -> Self;
    fn root_hash(&self) -> Digest;
    /// What changed going from `self` to `other`.
    fn diff(&self, other: &Self) -> Diff;
    /// Heap the tree holds, estimated by walking it. Allocator rounding is
    /// not counted.
    fn heap_bytes(&self) -> usize;
}

/// Heap held by one shared byte string: the reference counts and the bytes.
fn bytes_heap(bytes: &Bytes) -> usize {
    2 * size_of::<usize>() + bytes.len()
}

/// Merge-joins two runs of entries sorted the same way into changes.
fn join_sorted<'a, K: Ord>(
    old: impl IntoIterator<Item = (K, &'a Bytes, &'a Bytes)>,
    new: impl IntoIterator<Item = (K, &'a Bytes, &'a Bytes)>,
    out: &mut Vec<Change>,
) {
    let mut old = old.into_iter().peekable();
    let mut new = new.into_iter().peekable();
    loop {
        let order = match (old.peek(), new.peek()) {
            (None, None) => return,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some((a, ..)), Some((b, ..))) => a.cmp(b),
        };
        match order {
            std::cmp::Ordering::Less => {
                let (_, key, value) = old.next().expect("peeked");
                out.push(Change::Removed {
                    key: key.clone(),
                    value: value.clone(),
                });
            }
            std::cmp::Ordering::Greater => {
                let (_, key, value) = new.next().expect("peeked");
                out.push(Change::Added {
                    key: key.clone(),
                    value: value.clone(),
                });
            }
            std::cmp::Ordering::Equal => {
                let (_, key, old_value) = old.next().expect("peeked");
                let (_, _, new_value) = new.next().expect("peeked");
                if old_value != new_value {
                    out.push(Change::Modified {
                        key: key.clone(),
                        old: old_value.clone(),
                        new: new_value.clone(),
                    });
                }
            }
        }
    }
}
