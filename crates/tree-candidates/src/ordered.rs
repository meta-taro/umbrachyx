//! The node shape shared by the two ordered candidates, the prolly tree and
//! the B+ tree. They differ only in where a run of entries is cut into nodes.

use std::cmp::Ordering;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::hash::NodeHasher;
use crate::{Bytes, Change, Diff, Digest, Tree, bytes_heap};

pub type Entry = (Bytes, Bytes);

#[derive(Debug)]
pub enum Node {
    Leaf { entries: Vec<Entry>, hash: Digest },
    Internal { children: Vec<Child>, hash: Digest },
}

#[derive(Debug, Clone)]
pub struct Child {
    /// The largest key under this child, to route lookups without loading it.
    last_key: Bytes,
    node: Arc<Node>,
}

impl Child {
    fn of(node: Arc<Node>) -> Self {
        let last_key = node.last_key().expect("only the root can be empty").clone();
        Self { last_key, node }
    }

    pub fn last_key(&self) -> &Bytes {
        &self.last_key
    }
}

impl Node {
    fn leaf(entries: Vec<Entry>) -> Arc<Self> {
        let mut hasher = NodeHasher::new(0);
        for (key, value) in &entries {
            hasher.field(key);
            hasher.field(value);
        }
        Arc::new(Self::Leaf {
            hash: hasher.finish(),
            entries,
        })
    }

    fn internal(children: Vec<Child>) -> Arc<Self> {
        let mut hasher = NodeHasher::new(1);
        for child in &children {
            hasher.field(child.node.hash());
        }
        Arc::new(Self::Internal {
            hash: hasher.finish(),
            children,
        })
    }

    fn hash(&self) -> &Digest {
        match self {
            Self::Leaf { hash, .. } | Self::Internal { hash, .. } => hash,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Leaf { entries, .. } => entries.len(),
            Self::Internal { children, .. } => children.len(),
        }
    }

    fn last_key(&self) -> Option<&Bytes> {
        match self {
            Self::Leaf { entries, .. } => entries.last().map(|(key, _)| key),
            Self::Internal { children, .. } => children.last().map(|child| &child.last_key),
        }
    }

    fn heap_bytes(&self) -> usize {
        let own = size_of::<Self>() + 2 * size_of::<usize>();
        own + match self {
            Self::Leaf { entries, .. } => {
                entries.capacity() * size_of::<Entry>()
                    + entries
                        .iter()
                        .map(|(key, value)| bytes_heap(key) + bytes_heap(value))
                        .sum::<usize>()
            }
            // Keys in `last_key` share the leaf's allocation; only the
            // pointer, counted in the capacity, is extra.
            Self::Internal { children, .. } => {
                children.capacity() * size_of::<Child>()
                    + children
                        .iter()
                        .map(|child| child.node.heap_bytes())
                        .sum::<usize>()
            }
        }
    }
}

/// Where a run of entries or children is cut into nodes. `level` is 0 for
/// leaves and counts up towards the root.
pub trait Split {
    const NAME: &'static str;

    fn split_leaf(entries: Vec<Entry>) -> Vec<Vec<Entry>>;
    fn split_internal(children: Vec<Child>, level: u32) -> Vec<Vec<Child>>;
}

#[derive(Debug)]
pub struct OrderedTree<S> {
    root: Arc<Node>,
    /// Levels above the leaves.
    height: u32,
    split: PhantomData<S>,
}

impl<S> Clone for OrderedTree<S> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            height: self.height,
            split: PhantomData,
        }
    }
}

impl<S: Split> OrderedTree<S> {
    /// Stacks levels on top of `nodes`, which sit at `level`, until one node
    /// is left.
    fn raise(mut nodes: Vec<Arc<Node>>, mut level: u32) -> Self {
        while nodes.len() > 1 {
            level += 1;
            let children = nodes.into_iter().map(Child::of).collect();
            nodes = S::split_internal(children, level)
                .into_iter()
                .map(Node::internal)
                .collect();
        }
        match nodes.pop() {
            Some(root) => Self {
                root,
                height: level,
                split: PhantomData,
            },
            None => Self::empty(),
        }
    }

    /// Returns the nodes that replace `node`: one, or more if it was split.
    fn insert_into(node: &Node, level: u32, key: Bytes, value: Bytes) -> Vec<Arc<Node>> {
        match node {
            Node::Leaf { entries, .. } => {
                let mut entries = entries.clone();
                match entries.binary_search_by(|(k, _)| k.cmp(&key)) {
                    Ok(index) => entries[index].1 = value,
                    Err(index) => entries.insert(index, (key, value)),
                }
                S::split_leaf(entries).into_iter().map(Node::leaf).collect()
            }
            Node::Internal { children, .. } => {
                let index = children
                    .partition_point(|child| child.last_key < key)
                    .min(children.len() - 1);
                let replaced = Self::insert_into(&children[index].node, level - 1, key, value);
                let mut new_children = Vec::with_capacity(children.len() + replaced.len());
                new_children.extend_from_slice(&children[..index]);
                new_children.extend(replaced.into_iter().map(Child::of));
                new_children.extend_from_slice(&children[index + 1..]);
                S::split_internal(new_children, level)
                    .into_iter()
                    .map(Node::internal)
                    .collect()
            }
        }
    }
}

impl<S: Split> Tree for OrderedTree<S> {
    const NAME: &'static str = S::NAME;

    fn empty() -> Self {
        Self {
            root: Node::leaf(Vec::new()),
            height: 0,
            split: PhantomData,
        }
    }

    fn from_sorted(entries: Vec<(Bytes, Bytes)>) -> Self {
        debug_assert!(
            entries.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "entries must be sorted by key with no key twice"
        );
        let leaves = S::split_leaf(entries).into_iter().map(Node::leaf).collect();
        Self::raise(leaves, 0)
    }

    fn get(&self, key: &[u8]) -> Option<&Bytes> {
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

    fn insert(&self, key: Bytes, value: Bytes) -> Self {
        Self::raise(
            Self::insert_into(&self.root, self.height, key, value),
            self.height,
        )
    }

    fn root_hash(&self) -> Digest {
        *self.root.hash()
    }

    fn diff(&self, other: &Self) -> Diff {
        diff(&self.root, &other.root)
    }

    fn heap_bytes(&self) -> usize {
        self.root.heap_bytes()
    }
}

/// Walks both trees in key order, skipping any subtree that starts at the
/// same place on both sides with the same hash. Equal subtrees hold equal
/// entries, and a parent holds its children's hashes, so a shared child is
/// skipped without being read: only the regions around changes are.
fn diff(old: &Node, new: &Node) -> Diff {
    let mut old = Cursor::new(old);
    let mut new = Cursor::new(new);
    let mut changes = Vec::new();
    loop {
        if skip_shared(&mut old, &mut new) {
            continue;
        }
        if old.descend() | new.descend() {
            continue;
        }
        let order = match (old.entry(), new.entry()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((a, _)), Some((b, _))) => a.cmp(b),
        };
        match order {
            Ordering::Less => {
                let (key, value) = old.entry().expect("checked above");
                changes.push(Change::Removed {
                    key: key.clone(),
                    value: value.clone(),
                });
                old.next();
            }
            Ordering::Greater => {
                let (key, value) = new.entry().expect("checked above");
                changes.push(Change::Added {
                    key: key.clone(),
                    value: value.clone(),
                });
                new.next();
            }
            Ordering::Equal => {
                let (key, old_value) = old.entry().expect("checked above");
                let (_, new_value) = new.entry().expect("checked above");
                if old_value != new_value {
                    changes.push(Change::Modified {
                        key: key.clone(),
                        old: old_value.clone(),
                        new: new_value.clone(),
                    });
                }
                old.next();
                new.next();
            }
        }
    }
    Diff {
        changes,
        nodes_visited: old.visited + new.visited,
    }
}

fn skip_shared(old: &mut Cursor<'_>, new: &mut Cursor<'_>) -> bool {
    // Largest subtrees first, so a shared node is skipped whole rather than
    // child by child.
    for old_depth in old.starting_depths() {
        for new_depth in new.starting_depths() {
            if old.hash_at(old_depth) == new.hash_at(new_depth) {
                old.skip(old_depth);
                new.skip(new_depth);
                return true;
            }
        }
    }
    false
}

/// A position in key order: the path from the root to the current item.
/// Each frame holds a node that has been read and the index of the current
/// child or, in a leaf, entry. The current child of an internal node is not
/// read until the diff has to look inside it.
struct Cursor<'a> {
    stack: Vec<(&'a Node, usize)>,
    visited: usize,
}

impl<'a> Cursor<'a> {
    fn new(root: &'a Node) -> Self {
        let mut cursor = Self {
            stack: vec![(root, 0)],
            visited: 1,
        };
        cursor.settle();
        cursor
    }

    /// Reads the current child, if the cursor is on one. Returns whether it
    /// moved.
    fn descend(&mut self) -> bool {
        let Some(&(Node::Internal { children, .. }, index)) = self.stack.last() else {
            return false;
        };
        self.stack.push((&children[index].node, 0));
        self.visited += 1;
        true
    }

    /// Climbs out of finished nodes, so the cursor rests on an item or the
    /// stack is empty at the end.
    fn settle(&mut self) {
        while let Some(&(node, index)) = self.stack.last() {
            if index < node.len() {
                return;
            }
            self.stack.pop();
            if let Some(parent) = self.stack.last_mut() {
                parent.1 += 1;
            }
        }
    }

    fn entry(&self) -> Option<&'a Entry> {
        match self.stack.last()? {
            (Node::Leaf { entries, .. }, index) => entries.get(*index),
            (Node::Internal { .. }, _) => None,
        }
    }

    /// Moves past the current item.
    fn next(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            top.1 += 1;
        }
        self.settle();
    }

    /// The subtrees that start at the current position, shallowest
    /// (largest) first. A depth below the stack's length is a node that has
    /// been read; the stack's length itself is the unread current child.
    fn starting_depths(&self) -> std::ops::Range<usize> {
        let mut depth = self.stack.len();
        while depth > 0 && self.stack[depth - 1].1 == 0 {
            depth -= 1;
        }
        let on_child = matches!(self.stack.last(), Some((Node::Internal { .. }, _)));
        depth..self.stack.len() + usize::from(on_child)
    }

    fn hash_at(&self, depth: usize) -> &'a Digest {
        match self.stack.get(depth) {
            Some((node, _)) => node.hash(),
            None => match self.stack.last() {
                Some(&(Node::Internal { children, .. }, index)) => children[index].node.hash(),
                _ => unreachable!("only an internal node has a current child"),
            },
        }
    }

    /// Moves past the whole subtree at `depth`.
    fn skip(&mut self, depth: usize) {
        if depth < self.stack.len() {
            self.stack.truncate(depth + 1);
            let top = &mut self.stack[depth];
            top.1 = top.0.len();
            self.settle();
        } else {
            self.next();
        }
    }
}
