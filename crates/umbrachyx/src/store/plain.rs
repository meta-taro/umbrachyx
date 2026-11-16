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

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::tree::Bytes;

/// Enough parts that a reader and the writer seldom meet in one.
const PARTS: usize = 64;

type Part = HashMap<Bytes, Bytes>;

#[derive(Debug)]
pub struct PlainMap {
    /// Keyed once per process, so keys chosen to collide cannot pile into
    /// one part.
    hasher: RandomState,
    parts: Vec<RwLock<Part>>,
}

impl PlainMap {
    pub fn new() -> Self {
        Self {
            hasher: RandomState::new(),
            parts: (0..PARTS).map(|_| RwLock::new(Part::new())).collect(),
        }
    }

    fn part(&self, key: &[u8]) -> &RwLock<Part> {
        let hash = self.hasher.hash_one(key);
        // The remainder is below PARTS, so it fits whatever usize is.
        let at = usize::try_from(hash % PARTS as u64).unwrap_or(0);
        &self.parts[at]
    }

    /// What `key` holds, as it was stored.
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        read(self.part(key)).get(key).cloned()
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        read(self.part(key)).contains_key(key)
    }

    /// Sets `key` to `value`, or removes it for `None`.
    pub fn put(&self, key: &[u8], value: Option<Bytes>) {
        let mut part = write(self.part(key));
        match (value, part.get_mut(key)) {
            (Some(value), Some(held)) => *held = value,
            (Some(value), None) => {
                part.insert(Bytes::from(key), value);
            }
            (None, _) => {
                part.remove(key);
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
            keys.extend(part.keys().skip(skip).take(wanted).cloned());
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
                    .map(|(key, value)| (key.clone(), value.clone()))
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

        map.put(b"a", Some(Bytes::from(&b"1"[..])));
        map.put(b"a", Some(Bytes::from(&b"2"[..])));
        map.put(b"b", Some(Bytes::from(&b"3"[..])));
        map.put(b"b", None);
        map.put(b"never there", None);

        assert_eq!(map.get(b"a").as_deref(), Some(&b"2"[..]));
        assert_eq!(map.get(b"b"), None);
        assert!(map.contains(b"a") && !map.contains(b"b"));
        assert!(!map.is_empty());
    }

    #[test]
    fn entries_come_back_in_key_order() {
        let map = PlainMap::new();
        for key in ["m/3", "a/1", "z/9", "m/1"] {
            map.put(key.as_bytes(), Some(Bytes::from(key.as_bytes())));
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
