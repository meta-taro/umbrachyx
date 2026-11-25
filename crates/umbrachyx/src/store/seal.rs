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

use super::siv::Siv;
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
    /// Which key each subject the store has sealed for is sealed under,
    /// for the threads that seal without the store ([`seal_apart`]).
    /// Only what the store put here: a subject missing from it is one
    /// for the store to seal.
    subjects: RwLock<HashMap<Vec<u8>, Fingerprint>>,
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

    fn print_of(&self, subject: &[u8]) -> Option<Fingerprint> {
        let subjects = self.subjects.read().unwrap_or_else(PoisonError::into_inner);
        subjects.get(subject).copied()
    }

    fn note_subject(&self, subject: &[u8], print: Fingerprint) {
        let mut subjects = self
            .subjects
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if subjects.len() >= MAX_SUBJECTS_APART {
            subjects.clear();
        }
        subjects.insert(subject.to_vec(), print);
    }

    fn forget_subject(&self, subject: &[u8]) {
        self.subjects
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(subject);
    }
}

pub(crate) struct Sealer {
    keys: Keys<Box<dyn KeyStorage + Send>>,
    ring: Ring,
    /// The cipher of each key sealed with so far. Setting one up expands
    /// the key, which costs more than sealing a short value; and a key
    /// here is already in the ring, so sealing takes no lock.
    ciphers: HashMap<Fingerprint, Siv>,
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
                subjects: RwLock::new(HashMap::new()),
            }),
            ciphers: HashMap::new(),
        }
    }

    /// Every key held now, as a key file's bytes: what a backup keeps
    /// beside its copy of the log.
    pub(crate) fn key_file(&self) -> Result<Vec<u8>, KeyError> {
        // Not the keys still held with puts answered early: no record
        // a backup copies was sealed under one, and a backup that kept
        // one would keep it after this store has lost it and given its
        // subject another.
        self.keys.durable_entries()
    }

    pub(crate) fn ring(&self) -> Ring {
        Arc::clone(&self.ring)
    }

    /// Makes a key for every subject among `keys` that has none, all of
    /// them durable with one write, so that sealing the values of many new
    /// subjects does not write the key file once for each.
    pub(crate) fn make_keys_for<'a>(
        &mut self,
        keys: impl Iterator<Item = &'a [u8]>,
    ) -> Result<(), KeyError> {
        self.keys.make_keys(keys.map(subject))
    }

    /// As [`Sealer::make_keys_for`], without writing the key file: the
    /// keys are durable once [`Sealer::write_held_keys`] returns. Only
    /// for values that are not written before that.
    pub(crate) fn hold_keys_for<'a>(
        &mut self,
        keys: impl Iterator<Item = &'a [u8]>,
    ) -> Result<(), KeyError> {
        self.keys.make_keys_held(keys.map(subject))
    }

    /// Whether keys are in use that the key file does not hold yet.
    pub(crate) fn has_held_keys(&self) -> bool {
        self.keys.has_held()
    }

    /// Makes durable the keys [`Sealer::hold_keys_for`] made.
    pub(crate) fn write_held_keys(&mut self) -> Result<(), KeyError> {
        self.keys.write_held()
    }

    /// Seals `value` for `key`, making its subject's key first if needed.
    pub(crate) fn seal(&mut self, key: &[u8], value: &[u8]) -> Result<Vec<u8>, KeyError> {
        let subject = subject(key);
        let subject_key = self.keys.key_for(subject)?;
        let print = fingerprint(&subject_key);
        if !self.ciphers.contains_key(&print) {
            if self.ciphers.len() >= MAX_CIPHERS {
                self.ciphers.clear();
            }
            self.ring.write().insert(print, subject_key);
            // After the key is in the ring: whoever finds the subject
            // here finds its key there.
            self.ring.note_subject(subject, print);
            self.ciphers.insert(print, cipher_of(&subject_key));
        }
        let cipher = self.ciphers.get_mut(&print).expect("just put there");
        Ok(seal_with(cipher, &print, key, value))
    }
}

impl Sealer {
    /// The key with this fingerprint, if the key file holds it.
    pub(crate) fn key_of(&self, fingerprint: &Fingerprint) -> Option<&Key> {
        self.keys.by_fingerprint(fingerprint)
    }

    /// Keeps keys another store made, durable before any is in the ring:
    /// records sealed under them are written only after. A subject that
    /// has a key keeps it.
    pub(crate) fn take_keys(&mut self, keys: &[(&[u8], Key)]) -> Result<(), KeyError> {
        self.keys.put_keys(keys)?;
        let mut ring = self.ring.write();
        for (subject, key) in keys {
            if self.keys.get(subject) == Some(key) {
                ring.insert(fingerprint(key), *key);
            }
        }
        Ok(())
    }

    pub(crate) fn fingerprint_of(&self, subject: &[u8]) -> Option<Fingerprint> {
        self.keys.get(subject).map(fingerprint)
    }

    /// Takes the key out of the ring first, so nothing opens under it from
    /// here on even if the key file cannot be written; the store finishes
    /// the file when it next opens.
    pub(crate) fn erase(&mut self, subject: &[u8], key: &Fingerprint) -> Result<(), KeyError> {
        self.ring.forget_subject(subject);
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
/// Subjects whose key is named for the threads that seal without the
/// store, before starting over. One that is forgotten is sealed by the
/// store once more, which names it again.
const MAX_SUBJECTS_APART: usize = 65_536;

fn cipher_of(subject_key: &Key) -> Siv {
    Siv::new(subject_key)
}

fn seal_with(cipher: &mut Siv, print: &Fingerprint, key: &[u8], value: &[u8]) -> Vec<u8> {
    let sealed = cipher.seal(key, value);
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
    ciphers: HashMap<Fingerprint, Siv>,
}

thread_local! {
    static OPENERS: std::cell::RefCell<Openers> = std::cell::RefCell::default();
}

/// The value sealed in `sealed` for `key`, or `None` if its key is not in
/// the ring or the bytes do not open under it.
pub(crate) fn open(ring: &Ring, key: &[u8], sealed: &[u8]) -> Option<Bytes> {
    open_once(ring, key, sealed).map(Bytes::from)
}

/// As [`open`], giving the value in the bytes it was opened into: for a
/// reader that sends it on and keeps nothing, and would otherwise pay
/// for a copy it can share with nobody.
pub(crate) fn open_once(ring: &Ring, key: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
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
        cipher.open(key, &sealed[HEADER_LEN..])
    })
}

/// The ciphers a thread has sealed with without the store, by subject,
/// kept under the same rule as its [`Openers`].
#[derive(Default)]
struct Sealers {
    ring: u64,
    erasures: u64,
    ciphers: HashMap<Vec<u8>, (Fingerprint, Siv)>,
}

thread_local! {
    static SEALERS: std::cell::RefCell<Sealers> = std::cell::RefCell::default();
}

/// `value` sealed for `key` on this thread, without the store, exactly
/// as the store would seal it; or `None` if the store has not sealed for
/// the key's subject yet. Only the store makes keys, and it names a
/// subject for this once it has sealed for it.
///
/// So that many threads can seal at once: sealing is most of what a put
/// costs, and the store does one thing at a time.
pub(crate) fn seal_apart(ring: &Ring, key: &[u8], value: &[u8]) -> Option<Vec<u8>> {
    let subject = subject(key);
    SEALERS.with_borrow_mut(|sealers| {
        // Read before the ring, as `open` does: a cipher made for a key
        // erased after this is dropped on the next call.
        let erasures = ring.erasures.load(Ordering::SeqCst);
        let is_stale = sealers.ring != ring.id || sealers.erasures != erasures;
        if is_stale || sealers.ciphers.len() >= MAX_CIPHERS {
            sealers.ciphers.clear();
            sealers.ring = ring.id;
            sealers.erasures = erasures;
        }
        if !sealers.ciphers.contains_key(subject) {
            let print = ring.print_of(subject)?;
            let subject_key = *ring.read().get(&print)?;
            sealers
                .ciphers
                .insert(subject.to_vec(), (print, cipher_of(&subject_key)));
        }
        let (print, cipher) = sealers.ciphers.get_mut(subject)?;
        Some(seal_with(cipher, print, key, value))
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
