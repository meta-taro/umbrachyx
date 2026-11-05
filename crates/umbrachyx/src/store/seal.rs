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
pub(crate) type Ring = Arc<RwLock<HashMap<Fingerprint, Key>>>;

pub(crate) struct Sealer {
    keys: Keys<Box<dyn KeyStorage + Send>>,
    ring: Ring,
}

impl Sealer {
    pub(crate) fn new(keys: Keys<Box<dyn KeyStorage + Send>>) -> Self {
        let ring = keys.all().map(|key| (fingerprint(key), *key)).collect();
        Self {
            keys,
            ring: Arc::new(RwLock::new(ring)),
        }
    }

    pub(crate) fn ring(&self) -> Ring {
        Arc::clone(&self.ring)
    }

    /// Seals `value` for `key`, making its subject's key first if needed.
    pub(crate) fn seal(&mut self, key: &[u8], value: &[u8]) -> Result<Vec<u8>, KeyError> {
        let subject_key = self.keys.key_for(subject(key))?;
        self.ring
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fingerprint(&subject_key), subject_key);
        Ok(seal_with(&subject_key, key, value))
    }
}

fn seal_with(subject_key: &Key, key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut cipher = Aes256Siv::new_from_slice(subject_key).expect("keys are KEY_LEN bytes");
    let sealed = cipher
        .encrypt([key], value)
        .expect("AES-SIV seals any value with one header");
    let mut bytes = Vec::with_capacity(HEADER_LEN + sealed.len());
    bytes.push(SEALED);
    bytes.extend(fingerprint(subject_key));
    bytes.extend(sealed);
    bytes
}

/// The value sealed in `sealed` for `key`, or `None` if its key is not in
/// the ring or the bytes do not open under it.
pub(crate) fn open(ring: &Ring, key: &[u8], sealed: &[u8]) -> Option<Bytes> {
    let needed = key_fingerprint(sealed)?;
    let subject_key = *ring
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&needed)?;
    let mut cipher = Aes256Siv::new_from_slice(&subject_key).expect("keys are KEY_LEN bytes");
    cipher
        .decrypt([key], &sealed[HEADER_LEN..])
        .ok()
        .map(Bytes::from)
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
