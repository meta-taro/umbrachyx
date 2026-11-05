//! One encryption key per subject, such as a player, kept apart from the
//! log. The log is append-only and keeps every byte; the key file is the
//! one place where something is destroyed. Erasing a subject removes its
//! key from the file, after which every value written under it stays in
//! the log as ciphertext nobody can read.
//!
//! The file is a run of entries, each the subject's length (two bytes,
//! little-endian), the subject, the key, and a CRC-32 of the three. A new
//! key is appended and synced before anything is encrypted under it, so a
//! crash never leaves ciphertext whose key was lost. Erasing writes the
//! file again without the subject and renames it over the old one.
//!
//! Opening only reads. A torn entry at the end, left by a crash while a key
//! was appended, is cut when the next key is written: by then the store has
//! checked that every key its log uses is here, so damage that only looks
//! like a torn end is refused before anything is lost.
//!
//! Destroying a key this way removes it from the file the database reads.
//! It does not reach copies the operating system or the disk keep on its
//! own: blocks a solid-state disk has not reused yet, or backups.

use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::log::sync_parent_dir;

/// AES-SIV with AES-256 takes two 256-bit keys.
pub const KEY_LEN: usize = 64;
pub type Key = [u8; KEY_LEN];
/// Subjects are short names such as `player/42`. The limit also means a
/// damaged length is recognised as damage rather than read as a torn end.
pub const MAX_SUBJECT_LEN: usize = 1_024;
pub const FINGERPRINT_LEN: usize = 8;
/// Names a key without revealing it, so ciphertext can say which key it
/// needs and a missing key can be told apart from a damaged value.
pub type Fingerprint = [u8; FINGERPRINT_LEN];

const LEN_BYTES: usize = 2;
const CRC_BYTES: usize = 4;

/// Where the key file lives, so the simulation can replace the disk.
pub trait KeyStorage {
    /// The whole file, or nothing if there is none yet.
    fn read_all(&mut self) -> io::Result<Vec<u8>>;
    /// Adds `bytes` at the end, durable when this returns.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Replaces the whole file with `bytes`, durable when this returns. A
    /// crash leaves either the old file or the new one, never a mix.
    fn replace(&mut self, bytes: &[u8]) -> io::Result<()>;
}

/// A key file next to the log. Only the owner can read it on Unix.
#[derive(Debug)]
pub struct FileKeyStorage {
    path: PathBuf,
}

impl FileKeyStorage {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    fn create(path: &Path, truncate: bool) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true);
        if truncate {
            options.write(true).truncate(true);
        } else {
            options.append(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }
}

impl KeyStorage for FileKeyStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        match std::fs::read(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            read => read,
        }
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut file = Self::create(&self.path, false)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        // The file may be new; its directory entry has to survive too.
        sync_parent_dir(&self.path)
    }

    fn replace(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut name = self.path.clone().into_os_string();
        name.push(".new");
        let new = PathBuf::from(name);
        let mut file = Self::create(&new, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&new, &self.path)?;
        sync_parent_dir(&self.path)
    }
}

#[derive(Debug)]
pub enum KeyError {
    Io(io::Error),
    /// An entry before the last one is damaged. Keys after it may be in
    /// use, so they are not dropped.
    Damaged,
    /// Subjects are at most [`MAX_SUBJECT_LEN`] bytes.
    SubjectTooLong,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "key file: {error}"),
            Self::Damaged => write!(f, "key file: damaged before its last key"),
            Self::SubjectTooLong => write!(
                f,
                "key file: a subject is longer than {MAX_SUBJECT_LEN} bytes"
            ),
        }
    }
}

impl std::error::Error for KeyError {}

impl From<io::Error> for KeyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub struct Keys<S: KeyStorage> {
    storage: S,
    keys: HashMap<Vec<u8>, Key>,
    by_fingerprint: HashMap<Fingerprint, Vec<u8>>,
    /// The file ends in a torn entry, to be cut by the next write.
    torn: bool,
}

/// Shows how many keys there are, never the keys.
impl<S: KeyStorage> fmt::Debug for Keys<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keys")
            .field("subjects", &self.keys.len())
            .finish_non_exhaustive()
    }
}

impl<S: KeyStorage> Keys<S> {
    /// Reads every key, past a torn entry at the end. Changes nothing.
    pub fn open(mut storage: S) -> Result<Self, KeyError> {
        let bytes = storage.read_all()?;
        let (keys, valid) = parse(&bytes)?;
        let by_fingerprint = keys
            .iter()
            .map(|(subject, key)| (fingerprint(key), subject.clone()))
            .collect();
        Ok(Self {
            storage,
            keys,
            by_fingerprint,
            torn: valid < bytes.len(),
        })
    }

    pub fn get(&self, subject: &[u8]) -> Option<&Key> {
        self.keys.get(subject)
    }

    pub fn by_fingerprint(&self, fingerprint: &Fingerprint) -> Option<&Key> {
        self.by_fingerprint
            .get(fingerprint)
            .and_then(|subject| self.keys.get(subject))
    }

    /// The subject's key, made and made durable first if it has none.
    pub fn key_for(&mut self, subject: &[u8]) -> Result<Key, KeyError> {
        if let Some(key) = self.keys.get(subject) {
            return Ok(*key);
        }
        let mut key = [0; KEY_LEN];
        getrandom::getrandom(&mut key).map_err(|e| KeyError::Io(io::Error::other(e)))?;
        let added = entry(subject, &key)?;
        if self.torn {
            let mut bytes = self.entries()?;
            bytes.extend(added);
            self.storage.replace(&bytes)?;
            self.torn = false;
        } else {
            self.storage.append(&added)?;
        }
        self.keys.insert(subject.to_vec(), key);
        self.by_fingerprint
            .insert(fingerprint(&key), subject.to_vec());
        Ok(key)
    }

    /// Destroys the subject's key. Returns whether there was one. If the
    /// file cannot be written, the key is kept, so memory and file agree.
    pub fn erase(&mut self, subject: &[u8]) -> Result<bool, KeyError> {
        let Some(key) = self.keys.remove(subject) else {
            return Ok(false);
        };
        let replaced = self
            .entries()
            .and_then(|bytes| Ok(self.storage.replace(&bytes)?));
        if let Err(error) = replaced {
            self.keys.insert(subject.to_vec(), key);
            return Err(error);
        }
        self.by_fingerprint.remove(&fingerprint(&key));
        self.torn = false;
        Ok(true)
    }

    /// Every key in memory as the file's bytes, in subject order.
    fn entries(&self) -> Result<Vec<u8>, KeyError> {
        let mut subjects: Vec<&Vec<u8>> = self.keys.keys().collect();
        subjects.sort();
        let mut bytes = Vec::new();
        for subject in subjects {
            bytes.extend(entry(subject, &self.keys[subject])?);
        }
        Ok(bytes)
    }
}

pub fn fingerprint(key: &Key) -> Fingerprint {
    let hash = blake3::derive_key("umbrachyx 2026-11 subject key fingerprint", key);
    hash[..FINGERPRINT_LEN]
        .try_into()
        .expect("FINGERPRINT_LEN bytes")
}

fn entry(subject: &[u8], key: &Key) -> Result<Vec<u8>, KeyError> {
    if subject.len() > MAX_SUBJECT_LEN {
        return Err(KeyError::SubjectTooLong);
    }
    let len = u16::try_from(subject.len()).map_err(|_| KeyError::SubjectTooLong)?;
    let mut bytes = Vec::with_capacity(LEN_BYTES + subject.len() + KEY_LEN + CRC_BYTES);
    bytes.extend(len.to_le_bytes());
    bytes.extend(subject);
    bytes.extend(key);
    bytes.extend(crc32fast::hash(&bytes).to_le_bytes());
    Ok(bytes)
}

/// The keys, and how many bytes from the start are whole entries.
fn parse(bytes: &[u8]) -> Result<(HashMap<Vec<u8>, Key>, usize), KeyError> {
    let mut keys = HashMap::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(end) = entry_end(bytes, at)? else {
            // Shorter than its own length says: a torn append.
            break;
        };
        let body = &bytes[at..end - CRC_BYTES];
        let crc = u32::from_le_bytes(bytes[end - CRC_BYTES..end].try_into().expect("4 bytes"));
        if crc32fast::hash(body) != crc {
            if end == bytes.len() {
                break;
            }
            return Err(KeyError::Damaged);
        }
        let subject = &body[LEN_BYTES..body.len() - KEY_LEN];
        let key: Key = body[body.len() - KEY_LEN..]
            .try_into()
            .expect("KEY_LEN bytes");
        keys.insert(subject.to_vec(), key);
        at = end;
    }
    Ok((keys, at))
}

/// Where the entry starting at `at` ends, or `None` if not all of it is
/// there.
fn entry_end(bytes: &[u8], at: usize) -> Result<Option<usize>, KeyError> {
    let Some(len_bytes) = bytes.get(at..at + LEN_BYTES) else {
        return Ok(None);
    };
    let subject_len = usize::from(u16::from_le_bytes([len_bytes[0], len_bytes[1]]));
    if subject_len > MAX_SUBJECT_LEN {
        return Err(KeyError::Damaged);
    }
    let end = at + LEN_BYTES + subject_len + KEY_LEN + CRC_BYTES;
    Ok((end <= bytes.len()).then_some(end))
}
