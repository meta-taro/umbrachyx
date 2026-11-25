//! The values of tables that keep no history: one map for every branch,
//! overwritten in place.
//!
//! The thread that writes and the threads that read share it. A read
//! holds one part of the map for one lookup, so it can wait for a write
//! to that part for as long as an insert takes, never for a sync. That
//! is the trade against the tree and the latest-value map, which a write
//! copies so that a reader never waits at all: measured on the release
//! machine, copying was three quarters of a write's cost
//! (`docs/decisions.md`).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher, RandomState};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::tree::Bytes;

/// Enough parts that a reader and the writer seldom meet in one.
const PARTS: usize = 64;
/// Which bits of a key's hash choose its part: not the low ones, which
/// place it within the part, nor the top seven, which the part's table
/// compares first.
const PART_SHIFT: u32 = 40;

/// A value up to this long is kept in bytes of its own and overwritten
/// there; a longer one is shared with whoever reads it, and replaced
/// whole. Writing over a short value takes no memory and gives none
/// back; reading it copies it, under the part's lock, which is why a
/// long one is not held this way. On one laptop, writes sent 64 at a
/// time went up 6 to 19% for values of 32 and 128 bytes and stayed the
/// same at 512 and 1,024, and reads stayed the same at every length
/// (`bench/results/2026-11-26-laptop-plain-map/`).
const MAX_IN_PLACE: usize = 1024;

/// One part of the map, by the hash worked out once for the whole map.
type Part = HashMap<Held, Value, BuildHasherDefault<HashGiven>>;

#[derive(Debug)]
pub struct PlainMap {
    /// Keyed once per process, so keys chosen to collide cannot pile into
    /// one part, nor into one place of a part.
    hasher: RandomState,
    parts: Vec<RwLock<Part>>,
}

/// A key as a part holds it, with its hash.
#[derive(Debug)]
struct Held {
    hash: u64,
    key: Bytes,
}

/// A key asked for, with its hash.
struct Asked<'a> {
    hash: u64,
    key: &'a [u8],
}

/// What a part looks keys up by: a key held and a key asked for are the
/// same to it, so asking needs no key made to ask with.
trait Probe {
    fn hash(&self) -> u64;
    fn key(&self) -> &[u8];
}

impl Probe for Held {
    fn hash(&self) -> u64 {
        self.hash
    }

    fn key(&self) -> &[u8] {
        &self.key
    }
}

impl Probe for Asked<'_> {
    fn hash(&self) -> u64 {
        self.hash
    }

    fn key(&self) -> &[u8] {
        self.key
    }
}

impl<'a> Borrow<dyn Probe + 'a> for Held {
    fn borrow(&self) -> &(dyn Probe + 'a) {
        self
    }
}

impl Hash for Held {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl Hash for dyn Probe + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(Probe::hash(self));
    }
}

impl PartialEq for Held {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Held {}

impl PartialEq for dyn Probe + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for dyn Probe + '_ {}

/// Hands a part's table the hash a key came with. The map's keyed hasher
/// made it; hashing it again would only cost.
#[derive(Default)]
struct HashGiven(u64);

impl Hasher for HashGiven {
    fn write_u64(&mut self, hash: u64) {
        self.0 = hash;
    }

    /// Nothing here hashes anything but a hash; bytes are folded in all
    /// the same, so that a mistake would be slow rather than wrong.
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*byte);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// What a key holds, sealed or not, as it was stored.
#[derive(Debug)]
enum Value {
    InPlace(Vec<u8>),
    Shared(Bytes),
}

impl Value {
    fn of(bytes: &[u8]) -> Self {
        if bytes.len() <= MAX_IN_PLACE {
            Self::InPlace(bytes.to_vec())
        } else {
            Self::Shared(Bytes::from(bytes))
        }
    }

    /// Becomes `bytes`, in the bytes it has if they are its own and
    /// `bytes` fits them.
    fn set(&mut self, bytes: &[u8]) {
        match self {
            Self::InPlace(held) if bytes.len() <= MAX_IN_PLACE => {
                held.clear();
                held.extend_from_slice(bytes);
            }
            _ => *self = Self::of(bytes),
        }
    }

    fn bytes(&self) -> Bytes {
        match self {
            Self::InPlace(held) => Bytes::from(held.as_slice()),
            Self::Shared(held) => held.clone(),
        }
    }
}

impl PlainMap {
    pub fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            parts: (0..PARTS).map(|_| RwLock::new(Part::default())).collect(),
        }
    }

    /// `key` with its hash, and the part it is in.
    fn asked<'a>(&self, key: &'a [u8]) -> (Asked<'a>, &RwLock<Part>) {
        let hash = self.hasher.hash_one(key);
        // The remainder is below PARTS, so it fits whatever usize is.
        let at = usize::try_from((hash >> PART_SHIFT) % PARTS as u64).unwrap_or(0);
        (Asked { hash, key }, &self.parts[at])
    }

    /// What `key` holds, as it was stored.
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        let (asked, part) = self.asked(key);
        read(part).get(&asked as &dyn Probe).map(Value::bytes)
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        let (asked, part) = self.asked(key);
        read(part).contains_key(&asked as &dyn Probe)
    }

    /// Sets `key` to `value`, or removes it for `None`.
    pub fn put(&self, key: &[u8], value: Option<&[u8]>) {
        let (asked, part) = self.asked(key);
        let mut part = write(part);
        match (value, part.get_mut(&asked as &dyn Probe)) {
            (Some(value), Some(held)) => held.set(value),
            (Some(value), None) => {
                let held = Held {
                    hash: asked.hash,
                    key: Bytes::from(key),
                };
                part.insert(held, Value::of(value));
            }
            (None, _) => {
                part.remove(&asked as &dyn Probe);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.iter().all(|part| read(part).is_empty())
    }

    /// The `count` keys after the first `skip`, and whether more follow.
    /// The order means nothing: it changes when a key is added or removed
    /// and when the process restarts, so a place named before either may
    /// skip or repeat keys. Each part is read at a moment of its own.
    /// Finding the place counts whole parts, and walks only the one the
    /// place falls in.
    pub fn keys(&self, skip: usize, count: usize) -> (Vec<Bytes>, bool) {
        let mut skip = skip;
        let mut keys = Vec::new();
        for part in &self.parts {
            let part = read(part);
            if skip >= part.len() {
                skip -= part.len();
                continue;
            }
            let wanted = count - keys.len();
            if wanted == 0 {
                return (keys, true);
            }
            keys.extend(
                part.keys()
                    .skip(skip)
                    .take(wanted)
                    .map(|held| held.key.clone()),
            );
            if part.len() - skip > wanted {
                return (keys, true);
            }
            skip = 0;
        }
        (keys, false)
    }

    /// Every key and what it holds, in key order. Each part is read at a
    /// moment of its own, so writes made meanwhile may or may not show.
    pub fn sorted_entries(&self) -> Vec<(Bytes, Bytes)> {
        let mut entries: Vec<(Bytes, Bytes)> = self
            .parts
            .iter()
            .flat_map(|part| {
                read(part)
                    .iter()
                    .map(|(held, value)| (held.key.clone(), value.bytes()))
                    .collect::<Vec<_>>()
            })
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        entries
    }
}

/// A lock a panic elsewhere cannot leave unusable: a part is changed by
/// one insert or removal, never left half changed.
fn read(part: &RwLock<Part>) -> RwLockReadGuard<'_, Part> {
    part.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(part: &RwLock<Part>) -> RwLockWriteGuard<'_, Part> {
    part.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_put_overwrites_and_a_removal_forgets() {
        let map = PlainMap::new();
        assert!(map.is_empty());

        map.put(b"a", Some(&b"1"[..]));
        map.put(b"a", Some(&b"2"[..]));
        map.put(b"b", Some(&b"3"[..]));
        map.put(b"b", None);
        map.put(b"never there", None);

        assert_eq!(map.get(b"a").as_deref(), Some(&b"2"[..]));
        assert_eq!(map.get(b"b"), None);
        assert!(map.contains(b"a") && !map.contains(b"b"));
        assert!(!map.is_empty());
    }

    #[test]
    fn many_keys_are_each_found_again_and_none_other() {
        let map = PlainMap::new();
        let key = |n: u32| format!("session/{n}/token").into_bytes();
        for n in 0..10_000 {
            map.put(&key(n), Some(format!("t{n}").as_bytes()));
        }
        for n in (0..10_000).step_by(3) {
            map.put(&key(n), None);
        }

        for n in 0..10_000 {
            let held = map.get(&key(n));
            if n % 3 == 0 {
                assert_eq!(held, None, "key {n} was removed");
            } else {
                assert_eq!(held.as_deref(), Some(format!("t{n}").as_bytes()));
            }
        }
        assert_eq!(map.get(&key(10_000)), None);
        let (keys, has_more) = map.keys(0, 10_000);
        assert_eq!(keys.len(), 6_666);
        assert!(!has_more);
    }

    /// A value is written over in its own bytes while it is short, and
    /// kept whole once it is long: what is read back is the value last
    /// put, whichever way it is held and whatever was there before.
    #[test]
    fn a_value_of_any_length_replaces_one_of_any_other() {
        let map = PlainMap::new();
        let lens = [
            0,
            1,
            40,
            MAX_IN_PLACE,
            MAX_IN_PLACE + 1,
            70_000,
            3,
            MAX_IN_PLACE,
        ];
        for (round, before) in lens.into_iter().enumerate() {
            for (at, after) in lens.into_iter().enumerate() {
                let key = format!("cache/{round}/{at}").into_bytes();
                let first = vec![round as u8; before];
                let second = vec![at as u8 + 100; after];
                map.put(&key, Some(&first));
                assert_eq!(map.get(&key).as_deref(), Some(&first[..]));

                map.put(&key, Some(&second));

                assert_eq!(map.get(&key).as_deref(), Some(&second[..]));
            }
        }
    }

    /// A value handed out is the value as it was then: a put after it
    /// does not change what a reader already holds.
    #[test]
    fn a_value_read_does_not_change_when_the_key_is_put_again() {
        let map = PlainMap::new();
        for len in [5, MAX_IN_PLACE + 5] {
            map.put(b"cache/1/top", Some(&vec![1; len]));
            let read = map.get(b"cache/1/top").expect("is there");

            map.put(b"cache/1/top", Some(&vec![2; len]));

            assert_eq!(&read[..], &vec![1; len][..]);
        }
    }

    #[test]
    fn entries_come_back_in_key_order() {
        let map = PlainMap::new();
        for key in ["m/3", "a/1", "z/9", "m/1"] {
            map.put(key.as_bytes(), Some(key.as_bytes()));
        }

        let keys: Vec<Bytes> = map
            .sorted_entries()
            .into_iter()
            .map(|(key, _)| key)
            .collect();

        assert_eq!(
            keys,
            ["a/1", "m/1", "m/3", "z/9"].map(|key| Bytes::from(key.as_bytes()))
        );
    }
}
