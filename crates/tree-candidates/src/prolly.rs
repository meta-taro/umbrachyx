//! Prolly tree: a B+ tree whose node boundaries are chosen by the content,
//! as in Dolt and Noms.
//!
//! An entry ends its node when the hash of its key, salted by the level,
//! falls in one of `TARGET_FANOUT` buckets. The shape therefore depends only
//! on which keys are present, never on the order they arrived: two replicas
//! with the same data have the same root hash, and a diff between trees built
//! separately still skips everything they share.
//!
//! Boundaries depend on the key alone, not the value, so updating a value
//! never moves a boundary. Node size is unbounded in principle; runs without
//! a boundary are geometric, so a node of a few hundred entries is already
//! rare at 10 million keys.

use crate::hash::{key_hash, mix};
use crate::ordered::{Child, Entry, OrderedTree, Split};

const TARGET_FANOUT: u64 = 32;

#[derive(Debug)]
pub struct Prolly;

pub type ProllyTree = OrderedTree<Prolly>;

impl Split for Prolly {
    const NAME: &'static str = "prolly tree";

    fn split_leaf(entries: Vec<Entry>) -> Vec<Vec<Entry>> {
        cut_after(entries, |(key, _)| ends_node(key, 0))
    }

    fn split_internal(children: Vec<Child>, level: u32) -> Vec<Vec<Child>> {
        cut_after(children, |child| ends_node(child.last_key(), level))
    }
}

/// Each level gets its own salt. With one hash for every level, every key
/// that ends a leaf would also end its parent, and the tree would not narrow.
fn ends_node(key: &[u8], level: u32) -> bool {
    let salt = u64::from(level + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mix(key_hash(key).wrapping_add(salt)).is_multiple_of(TARGET_FANOUT)
}

fn cut_after<T>(items: Vec<T>, ends: impl Fn(&T) -> bool) -> Vec<Vec<T>> {
    let mut nodes = Vec::new();
    let mut current = Vec::new();
    for item in items {
        let is_end = ends(&item);
        current.push(item);
        if is_end {
            nodes.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        nodes.push(current);
    }
    nodes
}
