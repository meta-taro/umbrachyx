//! A persistent hash array mapped trie: the map the latest-value engine
//! keeps a table in.
//!
//! A key is placed by a hash of the key, 16 ways per level. An insert or
//! remove returns a new version and shares every untouched node with the
//! old one, so a branch is one pointer, as with the tree. Unlike the tree,
//! keys are in hash order, so there is no reading a range of keys in order;
//! the latest-value engine gives that up for a cheaper write and read.
//!
//! The hash is SipHash with a key drawn once per process, so keys chosen to
//! collide cannot pile into one leaf. Nothing about a map's layout leaves
//! the process: `sorted_entries` and `diff` give results in key order.
//!
//! Nodes carry no Merkle hash. A diff skips any subtree both sides share by
//! pointer, which is every untouched one when the two versions come from
//! the same line of history, as a branch and its base do. Maps built apart
//! still diff correctly, reading more.

use std::cmp::Ordering;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, OnceLock};

use crate::tree::{Bytes, Change, Diff};

const BITS_PER_LEVEL: u32 = 4;
/// Below this depth all 64 bits of the hash are used up; keys that collide
/// on every bit share a leaf however many there are.
const MAX_DEPTH: u32 = u64::BITS / BITS_PER_LEVEL;
/// A node holding at most this many keys is a leaf rather than a branch.
/// Whatever order keys came in, a node is a leaf exactly when it holds no
/// more than this, so the shape depends only on which keys are present.
const BUCKET: usize = 32;

/// Sorted by (hash, key).
type Entry = (u64, Bytes, Bytes);

#[derive(Debug)]
enum Node {
    Leaf(Vec<Entry>),
    /// Children sorted by the 4 bits of hash they stand for, none empty,
    /// holding `len` keys between them.
    Branch {
        children: Vec<(u8, Arc<Node>)>,
        len: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Hamt {
    root: Arc<Node>,
    len: usize,
}

impl Hamt {
    pub fn empty() -> Self {
        Self {
            root: Arc::new(Node::Leaf(Vec::new())),
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, key: &[u8]) -> Option<&Bytes> {
        let hash = key_hash(key);
        let mut node = &*self.root;
        let mut depth = 0;
        loop {
            match node {
                Node::Leaf(entries) => {
                    let index = entries
                        .binary_search_by(|entry| order(entry, hash, key))
                        .ok()?;
                    return Some(&entries[index].2);
                }
                Node::Branch { children, .. } => {
                    let index = children
                        .binary_search_by_key(&slot(hash, depth), |(slot, _)| *slot)
                        .ok()?;
                    node = &children[index].1;
                    depth += 1;
                }
            }
        }
    }

    /// Inserts or replaces, returning a new version. `self` is unchanged.
    pub fn insert(&self, key: Bytes, value: Bytes) -> Self {
        let (root, added) = insert(&self.root, 0, (key_hash(&key), key, value));
        Self {
            root,
            len: self.len + usize::from(added),
        }
    }

    /// Removes `key` if present, returning a new version.
    pub fn remove(&self, key: &[u8]) -> Self {
        match remove(&self.root, 0, key_hash(key), key) {
            Some(root) => Self {
                root,
                len: self.len - 1,
            },
            None => self.clone(),
        }
    }

    /// Every entry, in key order.
    pub fn sorted_entries(&self) -> Vec<(Bytes, Bytes)> {
        let mut entries = Vec::with_capacity(self.len);
        collect(&self.root, &mut entries, &mut 0);
        let mut entries: Vec<(Bytes, Bytes)> = entries
            .into_iter()
            .map(|(_, key, value)| (key, value))
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// What changed going from `self` to `other`, in key order.
    pub fn diff(&self, other: &Self) -> Diff {
        let mut changes = Vec::new();
        let mut nodes_visited = 0;
        diff_nodes(&self.root, &other.root, &mut changes, &mut nodes_visited);
        changes.sort_by(|a, b| a.key().cmp(b.key()));
        Diff {
            changes,
            nodes_visited,
        }
    }

    /// Every entry, in an order fixed by the content for the life of the
    /// process but meaning nothing: use it to visit or page through keys,
    /// never to show them in order.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            stack: vec![(&*self.root, 0)],
        }
    }

    /// Whether both are the very same version, not merely equal.
    pub fn same_root(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
    }

    /// Levels above the deepest leaf.
    #[cfg(test)]
    fn depth(&self) -> usize {
        fn depth_of(node: &Node) -> usize {
            match node {
                Node::Leaf(_) => 0,
                Node::Branch { children, .. } => {
                    1 + children
                        .iter()
                        .map(|(_, child)| depth_of(child))
                        .max()
                        .unwrap_or(0)
                }
            }
        }
        depth_of(&self.root)
    }
}

pub struct Iter<'a> {
    /// The nodes being walked, and the next entry or child of each.
    stack: Vec<(&'a Node, usize)>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a Bytes, &'a Bytes);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (node, at) = self.stack.last_mut()?;
            let node: &'a Node = node;
            match node {
                Node::Leaf(entries) => {
                    if let Some((_, key, value)) = entries.get(*at) {
                        *at += 1;
                        return Some((key, value));
                    }
                }
                Node::Branch { children, .. } => {
                    if let Some((_, child)) = children.get(*at) {
                        *at += 1;
                        self.stack.push((child, 0));
                        continue;
                    }
                }
            }
            self.stack.pop();
        }
    }
}

fn key_hash(key: &[u8]) -> u64 {
    static HASHER: OnceLock<RandomState> = OnceLock::new();
    HASHER.get_or_init(RandomState::new).hash_one(key)
}

fn slot(hash: u64, depth: u32) -> u8 {
    let shift = u64::BITS - BITS_PER_LEVEL * (depth + 1);
    // Masked to 4 bits, so it fits.
    ((hash >> shift) & 0xF) as u8
}

fn order(entry: &Entry, hash: u64, key: &[u8]) -> Ordering {
    (entry.0, &*entry.1).cmp(&(hash, key))
}

fn len_of(node: &Node) -> usize {
    match node {
        Node::Leaf(entries) => entries.len(),
        Node::Branch { len, .. } => *len,
    }
}

/// The node holding exactly `entries`, sorted by (hash, key), at `depth`.
fn build(entries: Vec<Entry>, depth: u32) -> Arc<Node> {
    if entries.len() <= BUCKET || depth == MAX_DEPTH {
        return Arc::new(Node::Leaf(entries));
    }
    let len = entries.len();
    let mut groups: Vec<(u8, Vec<Entry>)> = Vec::new();
    for entry in entries {
        let slot = slot(entry.0, depth);
        match groups.last_mut() {
            Some((last, group)) if *last == slot => group.push(entry),
            _ => groups.push((slot, vec![entry])),
        }
    }
    let children = groups
        .into_iter()
        .map(|(slot, group)| (slot, build(group, depth + 1)))
        .collect();
    Arc::new(Node::Branch { children, len })
}

/// The node with `entry` in it, and whether its key is new.
fn insert(node: &Node, depth: u32, entry: Entry) -> (Arc<Node>, bool) {
    match node {
        Node::Leaf(entries) => {
            let mut entries = entries.clone();
            let added = match entries.binary_search_by(|e| order(e, entry.0, &entry.1)) {
                Ok(index) => {
                    entries[index] = entry;
                    false
                }
                Err(index) => {
                    entries.insert(index, entry);
                    true
                }
            };
            (build(entries, depth), added)
        }
        Node::Branch { children, len } => {
            let slot = slot(entry.0, depth);
            let mut children = children.clone();
            let added = match children.binary_search_by_key(&slot, |(s, _)| *s) {
                Ok(index) => {
                    let (child, added) = insert(&children[index].1, depth + 1, entry);
                    children[index].1 = child;
                    added
                }
                Err(index) => {
                    children.insert(index, (slot, Arc::new(Node::Leaf(vec![entry]))));
                    true
                }
            };
            let len = len + usize::from(added);
            (Arc::new(Node::Branch { children, len }), added)
        }
    }
}

/// The node without `key`, or `None` if it was not there. A branch left
/// with few enough keys becomes a leaf again.
fn remove(node: &Node, depth: u32, hash: u64, key: &[u8]) -> Option<Arc<Node>> {
    match node {
        Node::Leaf(entries) => {
            let index = entries.binary_search_by(|e| order(e, hash, key)).ok()?;
            let mut entries = entries.clone();
            entries.remove(index);
            Some(Arc::new(Node::Leaf(entries)))
        }
        Node::Branch { children, len } => {
            let index = children
                .binary_search_by_key(&slot(hash, depth), |(s, _)| *s)
                .ok()?;
            let child = remove(&children[index].1, depth + 1, hash, key)?;
            let len = len - 1;
            let mut children = children.clone();
            if len_of(&child) == 0 {
                children.remove(index);
            } else {
                children[index].1 = child;
            }
            let node = Node::Branch { children, len };
            if len > BUCKET {
                return Some(Arc::new(node));
            }
            let mut entries = Vec::with_capacity(len);
            collect(&node, &mut entries, &mut 0);
            entries.sort_unstable_by(|a, b| order(a, b.0, &b.1));
            Some(Arc::new(Node::Leaf(entries)))
        }
    }
}

/// Every entry under `node`, in no particular order.
fn collect(node: &Node, out: &mut Vec<Entry>, visited: &mut usize) {
    *visited += 1;
    match node {
        Node::Leaf(entries) => out.extend(entries.iter().cloned()),
        Node::Branch { children, .. } => {
            for (_, child) in children {
                collect(child, out, visited);
            }
        }
    }
}

/// Both nodes sit at the same place in the trie, so children with the same
/// slot hold the same range of hashes and can be compared directly.
fn diff_nodes(old: &Arc<Node>, new: &Arc<Node>, out: &mut Vec<Change>, visited: &mut usize) {
    if Arc::ptr_eq(old, new) {
        return;
    }
    if let (Node::Branch { children: a, .. }, Node::Branch { children: b, .. }) = (&**old, &**new) {
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
                    join(gather(ca, visited), Vec::new(), out);
                    i += 1;
                }
                (Some((_, ca)), None) => {
                    join(gather(ca, visited), Vec::new(), out);
                    i += 1;
                }
                (_, Some((_, cb))) => {
                    join(Vec::new(), gather(cb, visited), out);
                    j += 1;
                }
                (None, None) => unreachable!("the loop runs while either side has children"),
            }
        }
        return;
    }
    // A leaf on either side: few entries, so compare them flat.
    let old = gather(old, visited);
    let new = gather(new, visited);
    join(old, new, out);
}

/// Every entry under `node`, sorted by (hash, key).
fn gather(node: &Node, visited: &mut usize) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(len_of(node));
    collect(node, &mut entries, visited);
    entries.sort_unstable_by(|a, b| order(a, b.0, &b.1));
    entries
}

/// The changes from `old` to `new`, both sorted by (hash, key).
fn join(old: Vec<Entry>, new: Vec<Entry>, out: &mut Vec<Change>) {
    let mut old = old.into_iter().peekable();
    let mut new = new.into_iter().peekable();
    loop {
        let step = match (old.peek(), new.peek()) {
            (None, None) => return,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => order(a, b.0, &b.1),
        };
        match step {
            Ordering::Less => {
                let (_, key, value) = old.next().expect("peeked");
                out.push(Change::Removed { key, value });
            }
            Ordering::Greater => {
                let (_, key, value) = new.next().expect("peeked");
                out.push(Change::Added { key, value });
            }
            Ordering::Equal => {
                let (_, key, old_value) = old.next().expect("peeked");
                let (_, _, new_value) = new.next().expect("peeked");
                if old_value != new_value {
                    out.push(Change::Modified {
                        key,
                        old: old_value,
                        new: new_value,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
