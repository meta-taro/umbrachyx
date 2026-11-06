//! The tree that holds the database's state: a persistent prolly tree with
//! BLAKE3 Merkle hashes.
//!
//! An insert or remove returns a new version and shares every untouched node
//! with the old one, so keeping a version costs one pointer. The shape
//! depends only on which keys are present, never on the order of changes,
//! so two trees with the same content have the same root hash and a diff
//! between any two reads only around what differs. `docs/decisions.md`
//! ("The tree") records why this structure was chosen.

use std::cmp::Ordering;
use std::sync::Arc;

mod diff;
mod node;

use node::{Child, Entry, Node, split_internal, split_leaf};

/// Keys and values are shared, not copied, between versions.
pub type Bytes = Arc<[u8]>;
pub type Digest = [u8; 32];

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
    /// Nodes read on both sides: the work the diff did.
    pub nodes_visited: usize,
}

#[derive(Debug, Clone)]
pub struct Tree {
    root: Arc<Node>,
    /// Levels above the leaves.
    height: u32,
    len: usize,
}

impl Tree {
    pub fn empty() -> Self {
        Self {
            root: Node::leaf(Vec::new()),
            height: 0,
            len: 0,
        }
    }

    /// Builds a tree from entries sorted by key, with no key twice.
    pub fn from_sorted(entries: Vec<(Bytes, Bytes)>) -> Self {
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "entries must be sorted by key with no key twice"
        );
        let len = entries.len();
        Self::raise(split_leaf(entries), 0, len)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn root_hash(&self) -> Digest {
        *self.root.hash()
    }

    pub fn get(&self, key: &[u8]) -> Option<&Bytes> {
        let mut node = &*self.root;
        loop {
            match node {
                Node::Leaf { entries, .. } => {
                    let index = entries.binary_search_by(|(k, _)| (**k).cmp(key)).ok()?;
                    return Some(&entries[index].1);
                }
                Node::Internal { children, .. } => {
                    let index = children.partition_point(|child| *child.last_key < *key);
                    node = &children.get(index)?.node;
                }
            }
        }
    }

    /// Inserts or replaces, returning a new version. `self` is unchanged.
    pub fn insert(&self, key: Bytes, value: Bytes) -> Self {
        self.insert_sorted(vec![(key, value)])
    }

    /// Inserts or replaces every entry, sorted by key with no key twice,
    /// returning a new version. Each node above a changed key is rebuilt and
    /// hashed once, however many of the keys land under it.
    pub fn insert_sorted(&self, entries: Vec<(Bytes, Bytes)>) -> Self {
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "entries must be sorted by key with no key twice"
        );
        if entries.is_empty() {
            return self.clone();
        }
        let (nodes, added) = insert_into(&self.root, self.height, &entries);
        Self::raise(nodes, self.height, self.len + added)
    }

    /// Removes `key` if present, returning a new version.
    pub fn remove(&self, key: &[u8]) -> Self {
        match remove_from(&self.root, self.height, key) {
            Some(nodes) => Self::raise(nodes, self.height, self.len - 1),
            None => self.clone(),
        }
    }

    /// Every entry, in key order.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            stack: vec![(&*self.root, 0)],
        }
    }

    /// Every entry with a key greater than `after`, in key order. Finding
    /// where to start reads one path down the tree, not the keys before it.
    pub fn iter_after(&self, after: &[u8]) -> Iter<'_> {
        let mut stack = Vec::new();
        let mut node = &*self.root;
        loop {
            match node {
                Node::Leaf { entries, .. } => {
                    let at = entries.partition_point(|(key, _)| **key <= *after);
                    stack.push((node, at));
                    return Iter { stack };
                }
                Node::Internal { children, .. } => {
                    // The first child holding a key past `after`; those
                    // before it hold none.
                    let at = children.partition_point(|child| *child.last_key <= *after);
                    stack.push((node, at + 1));
                    let Some(child) = children.get(at) else {
                        return Iter { stack };
                    };
                    node = &child.node;
                }
            }
        }
    }

    /// What changed going from `self` to `other`.
    pub fn diff(&self, other: &Self) -> Diff {
        diff::diff(&self.root, &other.root)
    }

    /// The keys that end a leaf, other than the last leaf. Removing one
    /// forces a merge; tests use this to reach every merge path.
    #[doc(hidden)]
    pub fn boundary_keys(&self) -> Vec<Bytes> {
        let mut keys = Vec::new();
        collect_leaf_ends(&self.root, &mut keys);
        keys.pop();
        keys
    }

    /// Stacks levels on top of `nodes`, which sit at `level`, until one node
    /// is left, then drops any root with a single child: the canonical shape
    /// has neither several nodes on top nor a top node with one child.
    fn raise(mut nodes: Vec<Arc<Node>>, mut level: u32, len: usize) -> Self {
        while nodes.len() > 1 {
            level += 1;
            let children = nodes.into_iter().map(Child::of).collect();
            nodes = split_internal(children, level);
        }
        let Some(mut root) = nodes.pop() else {
            return Self::empty();
        };
        while let Node::Internal { children, .. } = &*root
            && children.len() == 1
        {
            root = children[0].node.clone();
            level -= 1;
        }
        Self {
            root,
            height: level,
            len,
        }
    }
}

/// Returns the nodes that replace `node`, one or more if it was split, and
/// how many of `batch` were new keys. `batch` is sorted and every key in it
/// belongs under `node`. An insert never moves the last key of a node other
/// than the last one, so every node it returns but the last is closed.
fn insert_into(node: &Node, level: u32, batch: &[Entry]) -> (Vec<Arc<Node>>, usize) {
    match node {
        Node::Leaf { entries, .. } => {
            let (merged, added) = merge_sorted(entries, batch);
            (split_leaf(merged), added)
        }
        Node::Internal { children, .. } => {
            let mut new_children = Vec::with_capacity(children.len() + 1);
            let mut added = 0;
            let mut rest = batch;
            for (index, child) in children.iter().enumerate() {
                // Keys past the last child's last key go to the last child.
                let take = if index + 1 == children.len() {
                    rest.len()
                } else {
                    rest.partition_point(|(key, _)| *key <= child.last_key)
                };
                if take == 0 {
                    new_children.push(child.clone());
                    continue;
                }
                let (here, after) = rest.split_at(take);
                let (replaced, count) = insert_into(&child.node, level - 1, here);
                new_children.extend(replaced.into_iter().map(Child::of));
                added += count;
                rest = after;
            }
            (split_internal(new_children, level), added)
        }
    }
}

/// Merges two sorted runs; on a key in both, `new` wins. Also returns how
/// many keys of `new` were not in `old`.
fn merge_sorted(old: &[Entry], new: &[Entry]) -> (Vec<Entry>, usize) {
    let mut merged = Vec::with_capacity(old.len() + new.len());
    let (mut i, mut j, mut added) = (0, 0, 0);
    while i < old.len() && j < new.len() {
        match old[i].0.cmp(&new[j].0) {
            Ordering::Less => {
                merged.push(old[i].clone());
                i += 1;
            }
            Ordering::Greater => {
                merged.push(new[j].clone());
                j += 1;
                added += 1;
            }
            Ordering::Equal => {
                merged.push(new[j].clone());
                i += 1;
                j += 1;
            }
        }
    }
    merged.extend_from_slice(&old[i..]);
    merged.extend_from_slice(&new[j..]);
    (merged, added + new.len() - j)
}

/// Returns the nodes that replace `node`, or `None` if `key` is not under
/// it. Removing the key that ends a node leaves that node open; it is merged
/// with the node to its right, here if that node is a sibling, or by an
/// ancestor if not. The last node returned may still be open.
fn remove_from(node: &Node, level: u32, key: &[u8]) -> Option<Vec<Arc<Node>>> {
    match node {
        Node::Leaf { entries, .. } => {
            let index = entries.binary_search_by(|(k, _)| (**k).cmp(key)).ok()?;
            let mut entries = entries.clone();
            entries.remove(index);
            Some(split_leaf(entries))
        }
        Node::Internal { children, .. } => {
            let index = children.partition_point(|child| *child.last_key < *key);
            let child = children.get(index)?;
            let mut replaced = remove_from(&child.node, level - 1, key)?;
            let mut rest = &children[index + 1..];
            if let (Some(last), Some(next)) = (replaced.last(), rest.first())
                && !last.is_closed(level - 1)
            {
                let merged = merge(last, &next.node, level - 1);
                replaced.pop();
                replaced.extend(merged);
                rest = &rest[1..];
            }
            let mut new_children = Vec::with_capacity(children.len() + replaced.len());
            new_children.extend_from_slice(&children[..index]);
            new_children.extend(replaced.into_iter().map(Child::of));
            new_children.extend_from_slice(rest);
            Some(split_internal(new_children, level))
        }
    }
}

/// Joins an open node with the node to its right, both at `level`, and cuts
/// the result again. Inside, only the seam can be open, so the merge goes
/// down the seam: the last child of `left` with the first child of `right`.
fn merge(left: &Node, right: &Node, level: u32) -> Vec<Arc<Node>> {
    match (left, right) {
        (Node::Leaf { entries: a, .. }, Node::Leaf { entries: b, .. }) => {
            split_leaf(a.iter().chain(b).cloned().collect())
        }
        (Node::Internal { children: a, .. }, Node::Internal { children: b, .. }) => {
            let (left_last, left_rest) = a.split_last().expect("internal nodes have children");
            let mut children: Vec<Child> = left_rest.to_vec();
            if left_last.node.is_closed(level - 1) {
                children.push(left_last.clone());
                children.extend_from_slice(b);
            } else {
                let (right_first, right_rest) =
                    b.split_first().expect("internal nodes have children");
                children.extend(
                    merge(&left_last.node, &right_first.node, level - 1)
                        .into_iter()
                        .map(Child::of),
                );
                children.extend_from_slice(right_rest);
            }
            split_internal(children, level)
        }
        _ => unreachable!("nodes on one level are all leaves or all internal"),
    }
}

fn collect_leaf_ends(node: &Node, keys: &mut Vec<Bytes>) {
    match node {
        Node::Leaf { entries, .. } => keys.extend(entries.last().map(|(key, _)| key.clone())),
        Node::Internal { children, .. } => {
            for child in children {
                collect_leaf_ends(&child.node, keys);
            }
        }
    }
}

/// Walks a tree in key order. Each frame is a node and the index of the
/// next child or entry to visit in it.
pub struct Iter<'a> {
    stack: Vec<(&'a Node, usize)>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a Bytes, &'a Bytes);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (node, index) = self.stack.last_mut()?;
            let node: &'a Node = node;
            let at = *index;
            *index += 1;
            match node {
                Node::Leaf { entries, .. } => match entries.get(at) {
                    Some((key, value)) => return Some((key, value)),
                    None => {
                        self.stack.pop();
                    }
                },
                Node::Internal { children, .. } => match children.get(at) {
                    Some(child) => self.stack.push((&child.node, 0)),
                    None => {
                        self.stack.pop();
                    }
                },
            }
        }
    }
}
