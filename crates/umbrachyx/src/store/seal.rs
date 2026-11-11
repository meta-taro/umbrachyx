//! Encrypting values under their subject's key, at the store's edge: values
//! are sealed as they come into a commit and opened as they go out of a
//! read. Everything between, the log, the tree, history, diffs and merges,
//! holds only sealed values.
//!
//! Sealing is deterministic (AES-SIV, RFC 5297): one value under one key
//! always seals to the same bytes. That is what lets the tree, diffs,
//! three-way merges and undo keep comparing values as they did. What it
//! gives away is whether a key's value is the same as before, which the
//! shape of the history shows anyway. The key's own bytes are bound in as
//! associated data, so a sealed value cannot be moved under another key.
//!
//! A sealed value is a format byte, the fingerprint of the key it needs,
//! and the AES-SIV output.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use aes_siv::KeyInit;
use aes_siv::siv::Aes256Siv;

use crate::keys::{FINGERPRINT_LEN, Fingerprint, Key, KeyError, KeyStorage, Keys, fingerprint};
use crate::tree::Bytes;

const SEALED: u8 = 1;
const HEADER_LEN: usize = 1 + FINGERPRINT_LEN;

/// Every key there is, by fingerprint, shared with readers on other
/// threads. Erasing a subject takes its key out of here as well as out of
/// the key file, so no reader can open its values afterwards.
pub(crate) type Ring = Arc<Keyring>;

/// The keys, and what lets a reading thread keep its ciphers without
/// taking the lock on every read: the ring's own number, and how many of
/// its keys have been erased. A thread's ciphers are good only for the
/// ring and the count they were set up under.
pub(crate) struct Keyring {
    id: u64,
    erasures: AtomicU64,
    keys: RwLock<HashMap<Fingerprint, Key>>,
}

/// Never the keys themselves.
impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keyring({})", self.id)
    }
}

/// The next ring's number. Never reused, unlike an address.
static NEXT_RING: AtomicU64 = AtomicU64::new(1);

impl Keyring {
    pub(crate) fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<Fingerprint, Key>> {
        self.keys.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<Fingerprint, Key>> {
        self.keys.write().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct Sealer {
    keys: Keys<Box<dyn KeyStorage + Send>>,
    ring: Ring,
    /// The cipher of each key sealed with so far. Setting one up expands
    /// the key, which costs more than sealing a short value; and a key
    /// here is already in the ring, so sealing takes no lock.
    ciphers: HashMap<Fingerprint, Aes256Siv>,
}

impl Sealer {
    pub(crate) fn new(keys: Keys<Box<dyn KeyStorage + Send>>) -> Self {
        let ring = keys.all().map(|key| (fingerprint(key), *key)).collect();
        Self {
            keys,
            ring: Arc::new(Keyring {
                id: NEXT_RING.fetch_add(1, Ordering::Relaxed),
                erasures: AtomicU64::new(0),
                keys: RwLock::new(ring),
            }),
            ciphers: HashMap::new(),
        }
    }

    pub(crate) fn ring(&self) -> Ring {
        Arc::clone(&self.ring)
    }

    /// Seals `value` for `key`, making its subject's key first if needed.
    pub(crate) fn seal(&mut self, key: &[u8], value: &[u8]) -> Result<Vec<u8>, KeyError> {
        let subject_key = self.keys.key_for(subject(key))?;
        let print = fingerprint(&subject_key);
        if !self.ciphers.contains_key(&print) {
            if self.ciphers.len() >= MAX_CIPHERS {
                self.ciphers.clear();
            }
            self.ring.write().insert(print, subject_key);
            self.ciphers.insert(print, cipher_of(&subject_key));
        }
        let cipher = self.ciphers.get_mut(&print).expect("just put there");
        Ok(seal_with(cipher, &print, key, value))
    }
}

impl Sealer {
    pub(crate) fn fingerprint_of(&self, subject: &[u8]) -> Option<Fingerprint> {
        self.keys.get(subject).map(fingerprint)
    }

    /// Takes the key out of the ring first, so nothing opens under it from
    /// here on even if the key file cannot be written; the store finishes
    /// the file when it next opens.
    pub(crate) fn erase(&mut self, subject: &[u8], key: &Fingerprint) -> Result<(), KeyError> {
        self.ring.write().remove(key);
        // After the key has left the ring, so a reader that sees the new
        // count and drops its ciphers cannot find the key there again.
        self.ring.erasures.fetch_add(1, Ordering::SeqCst);
        self.ciphers.remove(key);
        self.keys.erase(subject).map(drop)
    }

    /// Destroys the key with this fingerprint if the key file still has it.
    pub(crate) fn erase_fingerprint(&mut self, key: &Fingerprint) -> Result<(), KeyError> {
        let Some(subject) = self.keys.subject_of(key).map(<[u8]>::to_vec) else {
            return Ok(());
        };
        self.erase(&subject, key)
    }
}

/// Ciphers kept per sealer and per reading thread before starting over.
const MAX_CIPHERS: usize = 1_024;

fn cipher_of(subject_key: &Key) -> Aes256Siv {
    Aes256Siv::new_from_slice(subject_key).expect("keys are KEY_LEN bytes")
}

fn seal_with(cipher: &mut Aes256Siv, print: &Fingerprint, key: &[u8], value: &[u8]) -> Vec<u8> {
    let sealed = cipher
        .encrypt([key], value)
        .expect("AES-SIV seals any value with one header");
    let mut bytes = Vec::with_capacity(HEADER_LEN + sealed.len());
    bytes.push(SEALED);
    bytes.extend(print);
    bytes.extend(sealed);
    bytes
}

/// The ciphers a thread has opened values with, and the ring and its
/// count of erasures they were set up under. Reads take no lock on the
/// ring, which readers on many threads would otherwise all write to, and
/// an erasure still reaches every thread before its next read.
#[derive(Default)]
struct Openers {
    ring: u64,
    erasures: u64,
    ciphers: HashMap<Fingerprint, Aes256Siv>,
}

thread_local! {
    static OPENERS: std::cell::RefCell<Openers> = std::cell::RefCell::default();
}

/// The value sealed in `sealed` for `key`, or `None` if its key is not in
/// the ring or the bytes do not open under it.
pub(crate) fn open(ring: &Ring, key: &[u8], sealed: &[u8]) -> Option<Bytes> {
    let needed = key_fingerprint(sealed)?;
    OPENERS.with_borrow_mut(|openers| {
        // Read before the ring: if a key is erased after this, the cipher
        // made below is dropped on the next read.
        let erasures = ring.erasures.load(Ordering::SeqCst);
        let is_stale = openers.ring != ring.id || openers.erasures != erasures;
        if is_stale || openers.ciphers.len() >= MAX_CIPHERS {
            openers.ciphers.clear();
            openers.ring = ring.id;
            openers.erasures = erasures;
        }
        let cipher = match openers.ciphers.entry(needed) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(unknown) => {
                let subject_key = *ring.read().get(&needed)?;
                unknown.insert(cipher_of(&subject_key))
            }
        };
        cipher
            .decrypt([key], &sealed[HEADER_LEN..])
            .ok()
            .map(Bytes::from)
    })
}

/// Which key `sealed` needs, or `None` if it is not a sealed value.
pub(crate) fn key_fingerprint(sealed: &[u8]) -> Option<Fingerprint> {
    match sealed.split_first() {
        Some((&SEALED, rest)) if rest.len() >= FINGERPRINT_LEN => {
            rest[..FINGERPRINT_LEN].try_into().ok()
        }
        _ => None,
    }
}

/// The first two parts of a key: `player/42/gems` belongs to `player/42`.
/// A key with one part names no subject and shares the database's own key,
/// `""`, so it cannot be erased on its own; giving every such key a key of
/// its own would cost a sync of the key file per new key.
pub(crate) fn subject(key: &[u8]) -> &[u8] {
    let mut slashes = key
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| at);
    match (slashes.next(), slashes.next()) {
        (None, _) => b"",
        (Some(_), None) => key,
        (Some(_), Some(at)) => &key[..at],
    }
}

#[cfg(test)]
mod tests {
    use super::subject;

    #[test]
    fn a_subject_is_the_first_two_parts_of_a_key() {
        assert_eq!(subject(b"player/42/gems"), b"player/42");
        assert_eq!(subject(b"player/42/item/sword"), b"player/42");
        assert_eq!(subject(b"player/42"), b"player/42");
        assert_eq!(subject(b"config"), b"");
        assert_eq!(subject(b"key:000123"), b"");
    }
}
