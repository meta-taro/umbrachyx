//! Nodes, their hashes, and where a run of entries is cut into nodes.

use std::sync::{Arc, OnceLock};

use super::{Bytes, Digest};

pub(super) type Entry = (Bytes, Bytes);

/// About one entry in this many ends a node, on every level.
const TARGET_FANOUT: u64 = 32;

/// A node's hash is worked out the first time it is asked for, by a diff
/// or a merge, and kept. Most nodes a write makes are replaced by the next
/// write before anything compares them, and are never hashed.
#[derive(Debug)]
pub(super) enum Node {
    Leaf {
        leaf: Leaf,
        hash: OnceLock<Digest>,
    },
    Internal {
        children: Vec<Child>,
        hash: OnceLock<Digest>,
        index: OnceLock<Arc<KeyIndex>>,
    },
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

/// A leaf's entries: its keys, shared by every version of the leaf that
/// has the same keys, and its values in one buffer of its own.
///
/// Most writes change one value of a leaf and none of its keys. The new
/// leaf then takes the old one's keys with one reference and copies the
/// values as bytes. While a leaf held a counted pointer for each key and
/// each value, such a write counted every one of them up to make the new
/// leaf and down again when the old one went: about 130 counts for one
/// changed value, each on memory that had not been touched for a while.
/// On the machine releases are measured on, that was half of what the
/// thread that writes did (`docs/decisions.md`, "The tree").
#[derive(Debug)]
pub(super) struct Leaf {
    keys: Arc<Keys>,
    /// Every value, one after another.
    values: Box<[u8]>,
    /// Where each value ends in `values`.
    ends: Box<[usize]>,
}

/// A leaf's keys, with the index lookups search them by, built the first
/// time one reaches the leaf.
#[derive(Debug)]
struct Keys {
    keys: Box<[Bytes]>,
    index: OnceLock<KeyIndex>,
}

impl Leaf {
    pub fn len(&self) -> usize {
        self.keys.keys.len()
    }

    pub fn keys(&self) -> &[Bytes] {
        &self.keys.keys
    }

    pub fn value(&self, at: usize) -> &[u8] {
        let start = at.checked_sub(1).map_or(0, |before| self.ends[before]);
        &self.values[start..self.ends[at]]
    }

    /// The entry at `at`, if the leaf has that many.
    pub fn get(&self, at: usize) -> Option<(&Bytes, &[u8])> {
        Some((self.keys.keys.get(at)?, self.value(at)))
    }

    /// Every entry, in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&Bytes, &[u8])> {
        self.keys
            .keys
            .iter()
            .enumerate()
            .map(|(at, key)| (key, self.value(at)))
    }

    fn index(&self) -> &KeyIndex {
        self.keys
            .index
            .get_or_init(|| KeyIndex::new(self.keys.keys.iter().map(|key| &**key)))
    }

    /// Where `key` is, or where it would go, as `binary_search` answers.
    pub fn search(&self, key: &[u8]) -> Result<usize, usize> {
        self.index().search(key)
    }

    /// A leaf with the same keys, shared with this one, and the values at
    /// the places in `changed` replaced. `changed` is sorted by place.
    pub fn with_values(&self, changed: &[(usize, &[u8])]) -> Self {
        let mut values = Vec::with_capacity(self.values.len());
        let mut ends = Vec::with_capacity(self.len());
        let mut changed = changed.iter().peekable();
        for at in 0..self.len() {
            match changed.next_if(|(place, _)| *place == at) {
                Some((_, value)) => values.extend_from_slice(value),
                None => values.extend_from_slice(self.value(at)),
            }
            ends.push(values.len());
        }
        Self {
            keys: Arc::clone(&self.keys),
            values: values.into(),
            ends: ends.into(),
        }
    }
}

/// Gathers the entries of a leaf being made.
#[derive(Default)]
struct LeafBuilder {
    keys: Vec<Bytes>,
    values: Vec<u8>,
    ends: Vec<usize>,
}

impl LeafBuilder {
    fn push(&mut self, key: Bytes, value: &[u8]) {
        self.keys.push(key);
        self.values.extend_from_slice(value);
        self.ends.push(self.values.len());
    }

    /// The leaf of what was pushed, leaving the builder empty.
    fn finish(&mut self) -> Arc<Node> {
        let built = std::mem::take(self);
        Node::of_leaf(Leaf {
            keys: Arc::new(Keys {
                keys: built.keys.into(),
                index: OnceLock::new(),
            }),
            values: built.values.into(),
            ends: built.ends.into(),
        })
    }
}

impl Node {
    pub fn leaf(entries: Vec<Entry>) -> Arc<Self> {
        let mut builder = LeafBuilder::default();
        for (key, value) in entries {
            builder.push(key, &value);
        }
        builder.finish()
    }

    pub fn of_leaf(leaf: Leaf) -> Arc<Self> {
        Arc::new(Self::Leaf {
            leaf,
            hash: OnceLock::new(),
        })
    }

    pub fn internal(children: Vec<Child>) -> Arc<Self> {
        Arc::new(Self::Internal {
            children,
            hash: OnceLock::new(),
            index: OnceLock::new(),
        })
    }

    /// An internal node with `children`, whose last keys are exactly
    /// `like`'s: it takes over the key index `like` has built, if any.
    pub fn internal_with_keys_of(like: &Self, children: Vec<Child>) -> Arc<Self> {
        Arc::new(Self::Internal {
            children,
            hash: OnceLock::new(),
            index: like.built_index(),
        })
    }

    fn built_index(&self) -> OnceLock<Arc<KeyIndex>> {
        match self {
            Self::Internal { index, .. } => {
                index.get().cloned().map(OnceLock::from).unwrap_or_default()
            }
            Self::Leaf { .. } => OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub fn is_hashed(&self) -> bool {
        match self {
            Self::Leaf { hash, .. } | Self::Internal { hash, .. } => hash.get().is_some(),
        }
    }

    pub fn hash(&self) -> &Digest {
        match self {
            Self::Leaf { leaf, hash } => hash.get_or_init(|| {
                let mut hasher = NodeHasher::new(0);
                for (key, value) in leaf.entries() {
                    hasher.field(key);
                    hasher.field(value);
                }
                hasher.finish()
            }),
            Self::Internal { children, hash, .. } => hash.get_or_init(|| {
                let mut hasher = NodeHasher::new(1);
                for child in children {
                    hasher.field(child.node.hash());
                }
                hasher.finish()
            }),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Leaf { leaf, .. } => leaf.len(),
            Self::Internal { children, .. } => children.len(),
        }
    }

    pub fn last_key(&self) -> Option<&Bytes> {
        match self {
            Self::Leaf { leaf, .. } => leaf.keys().last(),
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
    cut_leaves(entries.iter().map(|(key, value)| (key.clone(), &**value)))
}

/// Cuts entries, in key order, into leaves: a leaf ends after each key
/// that ends a node. No entries make no leaf.
pub(super) fn cut_leaves<'a>(entries: impl Iterator<Item = (Bytes, &'a [u8])>) -> Vec<Arc<Node>> {
    let mut leaves = Vec::new();
    let mut builder = LeafBuilder::default();
    for (key, value) in entries {
        let is_end = ends_node(&key, 0);
        builder.push(key, value);
        if is_end {
            leaves.push(builder.finish());
        }
    }
    if !builder.keys.is_empty() {
        leaves.push(builder.finish());
    }
    leaves
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

    /// The index finds every key where a binary search of the keys does,
    /// and puts every missing key where that search would, including keys
    /// that are a prefix of their neighbours and the empty key.
    #[test]
    fn the_key_index_answers_as_a_binary_search_of_the_keys() {
        let keys: Vec<&[u8]> = vec![
            b"",
            b"a",
            b"ab",
            b"abc",
            b"abd",
            b"b",
            b"key:000001",
            b"key:0000010",
            b"z",
        ];
        let index = KeyIndex::new(keys.iter().copied());
        let probes: Vec<&[u8]> = vec![
            b"",
            b"0",
            b"a",
            b"aa",
            b"ab",
            b"abb",
            b"abc",
            b"abcd",
            b"abd",
            b"abe",
            b"b",
            b"key:000001",
            b"key:0000010",
            b"key:00000100",
            b"key:000002",
            b"y",
            b"z",
            b"zz",
        ];

        for probe in probes {
            assert_eq!(
                index.search(probe),
                keys.binary_search(&probe),
                "probe {:?}",
                String::from_utf8_lossy(probe)
            );
        }
    }

    /// Keys that share a long start, differ only after the eight bytes
    /// that follow it, end inside it, or hold zero bytes there: the places
    /// where comparing a number taken from each key could go wrong.
    #[test]
    fn the_key_index_agrees_with_a_binary_search_on_awkward_keys() {
        let sets: Vec<Vec<&[u8]>> = vec![
            vec![
                b"key:000000000017",
                b"key:000000000018",
                b"key:000000000113",
            ],
            vec![
                b"player/0001/gems/a",
                b"player/0001/gems/b",
                b"player/0001/gems/bb",
            ],
            vec![b"ab", b"ab\0", b"ab\0\0", b"ab\0\0x", b"abc"],
            vec![b"same-start-12345678-a", b"same-start-12345678-b"],
            vec![b"only"],
            vec![b"", b"\0", b"\0\0\0\0\0\0\0\0", b"\0\0\0\0\0\0\0\0\0"],
            vec![
                b"\xff\xff\xff\xff\xff\xff\xff\xff",
                b"\xff\xff\xff\xff\xff\xff\xff\xff\xff",
            ],
        ];
        for keys in sets {
            let index = KeyIndex::new(keys.iter().copied());
            let mut probes: Vec<Vec<u8>> = vec![Vec::new(), vec![0], vec![0xff; 12]];
            for key in &keys {
                probes.push(key.to_vec());
                probes.push([key, &b"\0"[..]].concat());
                probes.push([key, &b"z"[..]].concat());
                for cut in 0..key.len() {
                    probes.push(key[..cut].to_vec());
                    let mut changed = key.to_vec();
                    changed[cut] = changed[cut].wrapping_add(1);
                    probes.push(changed.clone());
                    changed[cut] = changed[cut].wrapping_sub(2);
                    probes.push(changed);
                }
            }
            for probe in probes {
                assert_eq!(
                    index.search(&probe),
                    keys.binary_search(&&probe[..]),
                    "probe {probe:?} among {keys:?}"
                );
            }
        }
    }

    #[test]
    fn an_empty_index_puts_every_key_first() {
        let index = KeyIndex::new(std::iter::empty::<&[u8]>());

        assert_eq!(index.search(b"anything"), Err(0));
    }

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

    fn is_hashed(node: &Node) -> bool {
        node.is_hashed()
    }

    #[test]
    fn a_node_is_hashed_only_when_its_hash_is_asked_for() {
        let key: Bytes = b"player/0001/gems".to_vec().into();
        let value: Bytes = b"7".to_vec().into();
        let leaf = Node::leaf(vec![(key, value)]);
        let internal = Node::internal(vec![Child::of(leaf.clone())]);
        assert!(!is_hashed(&leaf));
        assert!(!is_hashed(&internal));

        let first = *internal.hash();
        assert!(is_hashed(&internal));
        assert!(is_hashed(&leaf), "a parent's hash needs its children's");
        assert_eq!(internal.hash(), &first, "asking again gives the kept hash");
    }
}

/// A node's keys in one buffer, so a lookup searches memory that sits
/// together instead of following a pointer per key. Built the first time a
/// lookup reaches the node, like the hash.
///
/// The keys of a node are neighbours, so they start alike. That shared
/// start is kept once, and beside each key the eight bytes after it as one
/// number: a lookup compares numbers, a few cache lines for the whole
/// node, and reads a key's bytes only where two numbers are equal.
#[derive(Debug)]
pub(super) struct KeyIndex {
    bytes: Vec<u8>,
    ends: Vec<u32>,
    /// How many bytes every key starts with in common.
    shared: usize,
    /// Per key, the bytes after the shared start as a number.
    heads: Vec<u64>,
}

/// The first eight bytes of `rest` as a number that orders as the bytes
/// do; a shorter `rest` counts as followed by zeros, which orders it no
/// later than anything it starts.
fn head(rest: &[u8]) -> u64 {
    let mut bytes = [0; 8];
    let taken = rest.len().min(8);
    bytes[..taken].copy_from_slice(&rest[..taken]);
    u64::from_be_bytes(bytes)
}

impl KeyIndex {
    fn new<'a>(keys: impl ExactSizeIterator<Item = &'a [u8]>) -> Self {
        let mut ends = Vec::with_capacity(keys.len());
        let mut bytes = Vec::new();
        for key in keys {
            bytes.extend_from_slice(key);
            ends.push(u32::try_from(bytes.len()).expect("a node's keys fit in 4 GiB"));
        }
        let mut index = Self {
            bytes,
            ends,
            shared: 0,
            heads: Vec::new(),
        };
        index.shared = index.shared_start();
        index.heads = (0..index.ends.len())
            .map(|at| head(&index.key(at)[index.shared..]))
            .collect();
        index
    }

    /// The keys are sorted, so what the first and the last start with in
    /// common, every key between them starts with too.
    fn shared_start(&self) -> usize {
        let Some(last) = self.ends.len().checked_sub(1) else {
            return 0;
        };
        let (first, last) = (self.key(0), self.key(last));
        first.iter().zip(last).take_while(|(a, b)| a == b).count()
    }

    fn key(&self, index: usize) -> &[u8] {
        let start = index
            .checked_sub(1)
            .map_or(0, |before| self.ends[before] as usize);
        &self.bytes[start..self.ends[index] as usize]
    }

    /// Where `key` is, or where it would go, as `binary_search` answers.
    pub fn search(&self, key: &[u8]) -> Result<usize, usize> {
        let Some(start) = self.bytes.get(..self.shared) else {
            return Err(0);
        };
        let Some(rest) = key.strip_prefix(start) else {
            // It sorts before every key or after every key.
            let after_all = key > start;
            return Err(if after_all { self.ends.len() } else { 0 });
        };
        let wanted = head(rest);
        let first = self.heads.partition_point(|&head| head < wanted);
        // Keys with the same number differ, if at all, past those eight
        // bytes or in how soon they end: their bytes decide.
        for at in first..self.heads.len() {
            if self.heads[at] != wanted {
                return Err(at);
            }
            match self.key(at)[self.shared..].cmp(rest) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => return Ok(at),
                std::cmp::Ordering::Greater => return Err(at),
            }
        }
        Err(self.heads.len())
    }
}

impl Node {
    /// The node's keys (a leaf) or last keys (an internal node), indexed.
    pub fn key_index(&self) -> &KeyIndex {
        match self {
            Self::Leaf { leaf, .. } => leaf.index(),
            Self::Internal {
                children, index, ..
            } => {
                index.get_or_init(|| Arc::new(KeyIndex::new(children.iter().map(|c| &*c.last_key))))
            }
        }
    }
}
