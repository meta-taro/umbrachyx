//! Hash array mapped trie with Merkle hashes: a key is placed by the hash of
//! the key, 16 ways per level, as in the Jellyfish Merkle tree.
//!
//! Like the prolly tree, its shape depends only on which keys are present.
//! Unlike it, keys are kept in hash order, so a range of keys is scattered
//! across the whole trie.

use std::sync::Arc;

use crate::hash::{NodeHasher, key_hash};
use crate::{Bytes, Change, Diff, Digest, Tree, bytes_heap, join_sorted};

const BITS_PER_LEVEL: u32 = 4;
/// Below this depth all 64 bits of the hash are used up; keys that collide
/// on every bit share a leaf however many there are.
const MAX_DEPTH: u32 = u64::BITS / BITS_PER_LEVEL;
/// A node holding at most this many keys is a leaf rather than a branch.
const BUCKET: usize = 32;

/// Sorted by (hash, key).
type HashedEntry = (u64, Bytes, Bytes);

#[derive(Debug)]
enum Node {
    Leaf {
        entries: Vec<HashedEntry>,
        hash: Digest,
    },
    /// Children sorted by the 4 bits of hash they stand for.
    Branch {
        children: Vec<(u8, Arc<Node>)>,
        hash: Digest,
    },
}

impl Node {
    fn leaf(entries: Vec<HashedEntry>) -> Arc<Self> {
        let mut hasher = NodeHasher::new(0);
        for (_, key, value) in &entries {
            hasher.field(key);
            hasher.field(value);
        }
        Arc::new(Self::Leaf {
            hash: hasher.finish(),
            entries,
        })
    }

    fn branch(children: Vec<(u8, Arc<Node>)>) -> Arc<Self> {
        let mut hasher = NodeHasher::new(1);
        for (slot, child) in &children {
            hasher.field(&[*slot]);
            hasher.field(child.hash());
        }
        Arc::new(Self::Branch {
            hash: hasher.finish(),
            children,
        })
    }

    /// The node holding exactly `entries` at `depth`. Depends only on the
    /// entries, which is what makes the trie's shape independent of order.
    fn build(entries: Vec<HashedEntry>, depth: u32) -> Arc<Self> {
        if entries.len() <= BUCKET || depth == MAX_DEPTH {
            return Self::leaf(entries);
        }
        let mut children: Vec<(u8, Vec<HashedEntry>)> = Vec::new();
        for entry in entries {
            let slot = slot(entry.0, depth);
            match children.last_mut() {
                Some((last, group)) if *last == slot => group.push(entry),
                _ => children.push((slot, vec![entry])),
            }
        }
        Self::branch(
            children
                .into_iter()
                .map(|(slot, group)| (slot, Self::build(group, depth + 1)))
                .collect(),
        )
    }

    fn hash(&self) -> &Digest {
        match self {
            Self::Leaf { hash, .. } | Self::Branch { hash, .. } => hash,
        }
    }

    fn insert(&self, depth: u32, entry: HashedEntry) -> Arc<Self> {
        match self {
            Self::Leaf { entries, .. } => {
                let mut entries = entries.clone();
                match entries.binary_search_by(|e| (e.0, &e.1).cmp(&(entry.0, &entry.1))) {
                    Ok(index) => entries[index] = entry,
                    Err(index) => entries.insert(index, entry),
                }
                Self::build(entries, depth)
            }
            Self::Branch { children, .. } => {
                let slot = slot(entry.0, depth);
                let mut children = children.clone();
                match children.binary_search_by_key(&slot, |(s, _)| *s) {
                    Ok(index) => children[index].1 = children[index].1.insert(depth + 1, entry),
                    Err(index) => children.insert(index, (slot, Self::leaf(vec![entry]))),
                }
                Self::branch(children)
            }
        }
    }

    fn collect<'a>(&'a self, out: &mut Vec<(u64, &'a Bytes, &'a Bytes)>, visited: &mut usize) {
        *visited += 1;
        match self {
            Self::Leaf { entries, .. } => out.extend(entries.iter().map(|(h, k, v)| (*h, k, v))),
            Self::Branch { children, .. } => {
                for (_, child) in children {
                    child.collect(out, visited);
                }
            }
        }
    }

    fn heap_bytes(&self) -> usize {
        let own = size_of::<Self>() + 2 * size_of::<usize>();
        own + match self {
            Self::Leaf { entries, .. } => {
                entries.capacity() * size_of::<HashedEntry>()
                    + entries
                        .iter()
                        .map(|(_, key, value)| bytes_heap(key) + bytes_heap(value))
                        .sum::<usize>()
            }
            Self::Branch { children, .. } => {
                children.capacity() * size_of::<(u8, Arc<Node>)>()
                    + children
                        .iter()
                        .map(|(_, child)| child.heap_bytes())
                        .sum::<usize>()
            }
        }
    }
}

fn slot(hash: u64, depth: u32) -> u8 {
    let shift = u64::BITS - BITS_PER_LEVEL * (depth + 1);
    ((hash >> shift) & 0xF) as u8
}

#[derive(Debug, Clone)]
pub struct HamtTree {
    root: Arc<Node>,
}

impl Tree for HamtTree {
    const NAME: &'static str = "HAMT";

    fn empty() -> Self {
        Self {
            root: Node::leaf(Vec::new()),
        }
    }

    fn from_sorted(entries: Vec<(Bytes, Bytes)>) -> Self {
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "entries must be sorted by key with no key twice"
        );
        let mut hashed: Vec<HashedEntry> = entries
            .into_iter()
            .map(|(key, value)| (key_hash(&key), key, value))
            .collect();
        hashed.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        Self {
            root: Node::build(hashed, 0),
        }
    }

    fn get(&self, key: &[u8]) -> Option<&Bytes> {
        let hash = key_hash(key);
        let mut node = &*self.root;
        let mut depth = 0;
        loop {
            match node {
                Node::Leaf { entries, .. } => {
                    let index = entries
                        .binary_search_by(|e| (e.0, &*e.1).cmp(&(hash, key)))
                        .ok()?;
                    return Some(&entries[index].2);
                }
                Node::Branch { children, .. } => {
                    let index = children
                        .binary_search_by_key(&slot(hash, depth), |(s, _)| *s)
                        .ok()?;
                    node = &children[index].1;
                    depth += 1;
                }
            }
        }
    }

    fn insert(&self, key: Bytes, value: Bytes) -> Self {
        Self {
            root: self.root.insert(0, (key_hash(&key), key, value)),
        }
    }

    fn root_hash(&self) -> Digest {
        *self.root.hash()
    }

    fn diff(&self, other: &Self) -> Diff {
        let mut changes = Vec::new();
        let mut nodes_visited = 0;
        diff_nodes(&self.root, &other.root, &mut changes, &mut nodes_visited);
        changes.sort_by(|a, b| a.key().cmp(b.key()));
        Diff {
            changes,
            nodes_visited,
        }
    }

    fn heap_bytes(&self) -> usize {
        self.root.heap_bytes()
    }
}

/// Both nodes sit at the same place in the trie, so children with the same
/// slot hold the same range of hashes and can be compared directly.
fn diff_nodes(old: &Node, new: &Node, out: &mut Vec<Change>, visited: &mut usize) {
    if old.hash() == new.hash() {
        return;
    }
    if let (Node::Branch { children: a, .. }, Node::Branch { children: b, .. }) = (old, new) {
        *visited += 2;
        let (mut i, mut j) = (0, 0);
        while i < a.len() || j < b.len() {
            match (a.get(i), b.get(j)) {
                (Some((sa, ca)), Some((sb, cb))) if sa == sb => {
                    diff_nodes(ca, cb, out, visited);
                    i += 1;
                    j += 1;
                }
                (Some((sa, ca)), Some((sb, _))) if sa < sb => {
                    removed_all(ca, out, visited);
                    i += 1;
                }
                (Some((_, ca)), None) => {
                    removed_all(ca, out, visited);
                    i += 1;
                }
                (_, Some((_, cb))) => {
                    added_all(cb, out, visited);
                    j += 1;
                }
                (None, None) => unreachable!("loop condition"),
            }
        }
        return;
    }
    // A leaf on either side: few entries, so compare them flat.
    let mut a = Vec::new();
    let mut b = Vec::new();
    old.collect(&mut a, visited);
    new.collect(&mut b, visited);
    join_sorted(
        a.into_iter().map(|(h, k, v)| ((h, k), k, v)),
        b.into_iter().map(|(h, k, v)| ((h, k), k, v)),
        out,
    );
}

fn removed_all(node: &Node, out: &mut Vec<Change>, visited: &mut usize) {
    let mut entries = Vec::new();
    node.collect(&mut entries, visited);
    join_sorted(entries.into_iter().map(|(h, k, v)| ((h, k), k, v)), [], out);
}

fn added_all(node: &Node, out: &mut Vec<Change>, visited: &mut usize) {
    let mut entries = Vec::new();
    node.collect(&mut entries, visited);
    join_sorted([], entries.into_iter().map(|(h, k, v)| ((h, k), k, v)), out);
}
