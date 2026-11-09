//! Nodes, their hashes, and where a run of entries is cut into nodes.

use std::sync::Arc;

use super::{Bytes, Digest};

pub(super) type Entry = (Bytes, Bytes);

/// About one entry in this many ends a node, on every level.
const TARGET_FANOUT: u64 = 32;

#[derive(Debug)]
pub(super) enum Node {
    Leaf { entries: Vec<Entry>, hash: Digest },
    Internal { children: Vec<Child>, hash: Digest },
}

#[derive(Debug, Clone)]
pub(super) struct Child {
    /// The largest key under this child, to route lookups without reading it.
    pub last_key: Bytes,
    pub node: Arc<Node>,
}

impl Child {
    pub fn of(node: Arc<Node>) -> Self {
        let last_key = node.last_key().expect("only the root can be empty").clone();
        Self { last_key, node }
    }
}

impl Node {
    pub fn leaf(entries: Vec<Entry>) -> Arc<Self> {
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

    pub fn internal(children: Vec<Child>) -> Arc<Self> {
        let mut hasher = NodeHasher::new(1);
        for child in &children {
            hasher.field(child.node.hash());
        }
        Arc::new(Self::Internal {
            hash: hasher.finish(),
            children,
        })
    }

    pub fn hash(&self) -> &Digest {
        match self {
            Self::Leaf { hash, .. } | Self::Internal { hash, .. } => hash,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Leaf { entries, .. } => entries.len(),
            Self::Internal { children, .. } => children.len(),
        }
    }

    pub fn last_key(&self) -> Option<&Bytes> {
        match self {
            Self::Leaf { entries, .. } => entries.last().map(|(key, _)| key),
            Self::Internal { children, .. } => children.last().map(|child| &child.last_key),
        }
    }

    /// Whether this node, at `level`, ends where the canonical shape would
    /// end it. Every node but the last on its level does: its last key ends
    /// a node on its own level and on every level below.
    pub fn is_closed(&self, level: u32) -> bool {
        self.last_key()
            .is_some_and(|key| (0..=level).all(|l| ends_node(key, l)))
    }
}

/// Cuts entries into leaves.
pub(super) fn split_leaf(entries: Vec<Entry>) -> Vec<Arc<Node>> {
    cut_after(entries, |(key, _)| ends_node(key, 0))
        .into_iter()
        .map(Node::leaf)
        .collect()
}

/// Cuts the children of `level` into nodes of that level.
pub(super) fn split_internal(children: Vec<Child>, level: u32) -> Vec<Arc<Node>> {
    cut_after(children, |child| ends_node(&child.last_key, level))
        .into_iter()
        .map(Node::internal)
        .collect()
}

/// An entry ends its node when the hash of its key, salted by the level,
/// falls in one bucket out of `TARGET_FANOUT`. Each level has its own salt:
/// with one hash for all, every key ending a leaf would also end its parent.
///
/// The key hash is not keyed, so keys chosen to collide could make nodes
/// large. That has to change before keys come from untrusted callers.
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

/// FNV-1a, then the SplitMix64 finalizer to spread the bits.
fn key_hash(key: &[u8]) -> u64 {
    let fnv = key.iter().fold(0xCBF2_9CE4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3)
    });
    mix(fnv)
}

/// The SplitMix64 finalizer. Distinct inputs give unrelated outputs, so
/// `mix(hash + salt)` acts as an independent hash per salt.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// BLAKE3 over a node's fields, each prefixed with its length so that no two
/// different nodes encode to the same bytes.
///
/// The fields are gathered into one buffer and hashed at once: BLAKE3 hashes
/// several 1 KiB chunks in parallel only when it is given them together, and
/// fed a few bytes at a time it compresses one block after another.
struct NodeHasher(Vec<u8>);

impl NodeHasher {
    fn new(tag: u8) -> Self {
        let mut bytes = Vec::with_capacity(4096);
        bytes.push(tag);
        Self(bytes)
    }

    fn field(&mut self, bytes: &[u8]) {
        self.0
            .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> Digest {
        *blake3::hash(&self.0).as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoding fed field by field, as nodes were first hashed: gathering
    /// it into one buffer must not change a single hash, or every commit
    /// name and every stored diff shortcut would change with it.
    fn hashed_field_by_field(tag: u8, fields: &[&[u8]]) -> Digest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[tag]);
        for field in fields {
            hasher.update(&(field.len() as u32).to_le_bytes());
            hasher.update(field);
        }
        *hasher.finalize().as_bytes()
    }

    #[test]
    fn node_hashes_match_the_field_by_field_encoding() {
        // Enough entries that the encoding spans several 1 KiB chunks.
        let entries: Vec<Entry> = (0..100_u32)
            .map(|i| {
                let key: Bytes = format!("player/{i:04}/gems").into_bytes().into();
                let value: Bytes = i.to_le_bytes().repeat(9).into();
                (key, value)
            })
            .collect();
        let fields: Vec<&[u8]> = entries
            .iter()
            .flat_map(|(key, value)| [&key[..], &value[..]])
            .collect();
        let leaf = Node::leaf(entries.clone());
        assert_eq!(leaf.hash(), &hashed_field_by_field(0, &fields));

        let children = vec![
            Child::of(leaf.clone()),
            Child::of(Node::leaf(entries[..1].to_vec())),
        ];
        let child_hashes: Vec<&[u8]> = children.iter().map(|c| &c.node.hash()[..]).collect();
        let internal = Node::internal(children.clone());
        assert_eq!(internal.hash(), &hashed_field_by_field(1, &child_hashes));
    }
}
