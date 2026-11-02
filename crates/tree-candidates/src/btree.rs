//! Persistent B+ tree with Merkle hashes: nodes split in half when full, and
//! an insert copies only the path from the root to the leaf.
//!
//! Its shape depends on the order of inserts. Two versions where one was
//! derived from the other share nodes and diff cheaply; two trees built
//! separately from the same data usually do not line up.

use crate::ordered::{Child, Entry, OrderedTree, Split};

const MAX_FANOUT: usize = 64;
/// How full a bulk-built node is, leaving room for inserts.
const BULK_FILL: usize = 48;

#[derive(Debug)]
pub struct BPlus;

pub type BPlusTree = OrderedTree<BPlus>;

impl Split for BPlus {
    const NAME: &'static str = "B+ tree";

    fn split_leaf(entries: Vec<Entry>) -> Vec<Vec<Entry>> {
        split_even(entries)
    }

    fn split_internal(children: Vec<Child>, _level: u32) -> Vec<Vec<Child>> {
        split_even(children)
    }
}

/// One node if it fits; otherwise nodes of about `BULK_FILL`, evened out. A
/// node that overflowed by one is cut in half.
fn split_even<T>(items: Vec<T>) -> Vec<Vec<T>> {
    if items.len() <= MAX_FANOUT {
        return if items.is_empty() {
            Vec::new()
        } else {
            vec![items]
        };
    }
    let count = items.len().div_ceil(BULK_FILL);
    let base = items.len() / count;
    let extra = items.len() % count;
    let mut items = items.into_iter();
    (0..count)
        .map(|i| items.by_ref().take(base + usize::from(i < extra)).collect())
        .collect()
}
