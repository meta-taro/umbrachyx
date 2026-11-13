//! One encryption key per subject, such as a player, kept apart from the
//! log. The log is append-only and keeps every byte; the key file is the
//! one place where something is destroyed. Erasing a subject removes its
//! key from the file, after which every value written under it stays in
//! the log as ciphertext nobody can read.
//!
//! The file starts with 16 bytes that say what it is: `UMBRAKEY`, the
//! layout's number (four bytes, little-endian) and a CRC-32 of the two.
//! After them it is a run of entries, each the subject's length (two bytes,
//! little-endian), the subject, the key, and a CRC-32 of the three. A new
//! key is appended and synced before anything is encrypted under it, so a
//! crash never leaves ciphertext whose key was lost. Erasing writes the
//! file again without the subject and renames it over the old one.
//!
//! Opening only reads. A torn entry at the end, left by a crash while a key
//! was appended, is cut when the next key is written: by then the store has
//! checked that every key its log uses is here, so damage that only looks
//! like a torn end is refused before anything is lost. Bytes that do not
//! make a whole entry are a torn end, garbage included, as long as no whole
//! entry follows them; with one after them they are damage, and refused.
//! The same goes for the start of the file: keys with no header that checks
//! out in front of them are refused, and a header of a later layout is
//! refused as that. A file with no header and no key gets its header with
//! the first key, in one write that replaces the file. A write that fails
//! leaves the same kind of leftover, a torn append or a key whose erasure
//! did not reach the file, and the next write replaces the whole file too.
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
/// Subjects are short names such as `player/42`.
pub const MAX_SUBJECT_LEN: usize = 1_024;
pub const FINGERPRINT_LEN: usize = 8;
/// Names a key without revealing it, so ciphertext can say which key it
/// needs and a missing key can be told apart from a damaged value.
pub type Fingerprint = [u8; FINGERPRINT_LEN];

const LEN_BYTES: usize = 2;
const CRC_BYTES: usize = 4;
const MAGIC: [u8; 8] = *b"UMBRAKEY";
/// Bumped when the file's layout changes.
const VERSION: u32 = 1;
const HEADER_LEN: usize = MAGIC.len() + 4 + CRC_BYTES;

/// What every key file starts with.
fn header() -> [u8; HEADER_LEN] {
    let mut bytes = [0; HEADER_LEN];
    bytes[..8].copy_from_slice(&MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..12]);
    bytes[12..].copy_from_slice(&crc.to_le_bytes());
    bytes
}

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

/// So a store can hold a key file without naming its type.
impl KeyStorage for Box<dyn KeyStorage + Send> {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        (**self).read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        (**self).append(bytes)
    }

    fn replace(&mut self, bytes: &[u8]) -> io::Result<()> {
        (**self).replace(bytes)
    }
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
    /// An entry is damaged and whole keys follow it. They may be in use, so
    /// they are not dropped.
    Damaged,
    /// Subjects are at most [`MAX_SUBJECT_LEN`] bytes.
    SubjectTooLong,
    /// The file says it is a key file in a layout this version does not
    /// know: one written by a later version.
    NewerFormat {
        version: u32,
    },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "key file: {error}"),
            Self::Damaged => write!(f, "key file: damaged before its last key"),
            Self::NewerFormat { version } => write!(
                f,
                "key file: in format {version}, written by a later version than this one"
            ),
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
    /// The file holds bytes memory has let go of: a torn entry at its end,
    /// or a key whose erasure did not reach it. The next write replaces it.
    stale: bool,
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
        let (keys, is_whole) = read_file(&bytes)?;
        let by_fingerprint = keys
            .iter()
            .map(|(subject, key)| (fingerprint(key), subject.clone()))
            .collect();
        Ok(Self {
            storage,
            keys,
            by_fingerprint,
            stale: !is_whole,
        })
    }

    pub fn get(&self, subject: &[u8]) -> Option<&Key> {
        self.keys.get(subject)
    }

    /// Every key, in no particular order.
    pub fn all(&self) -> impl Iterator<Item = &Key> {
        self.keys.values()
    }

    /// The subject whose key has this fingerprint.
    pub fn subject_of(&self, fingerprint: &Fingerprint) -> Option<&[u8]> {
        self.by_fingerprint.get(fingerprint).map(Vec::as_slice)
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
        self.make_keys(std::iter::once(subject))?;
        Ok(self.keys[subject])
    }

    /// Makes a key for each of `subjects` that has none, and makes them all
    /// durable with one write. Either all of them are in use afterwards or
    /// none is: a value sealed under a key that is not durable could never
    /// be opened again.
    pub fn make_keys<'a>(
        &mut self,
        subjects: impl Iterator<Item = &'a [u8]>,
    ) -> Result<(), KeyError> {
        let mut made: Vec<(&[u8], Key)> = Vec::new();
        let mut added = Vec::new();
        for subject in subjects {
            let is_new =
                !self.keys.contains_key(subject) && made.iter().all(|(other, _)| *other != subject);
            if !is_new {
                continue;
            }
            let mut key = [0; KEY_LEN];
            getrandom::getrandom(&mut key).map_err(|e| KeyError::Io(io::Error::other(e)))?;
            added.extend(entry(subject, &key)?);
            made.push((subject, key));
        }
        if made.is_empty() {
            return Ok(());
        }
        if self.stale {
            let mut bytes = self.entries()?;
            bytes.extend(added);
            self.storage.replace(&bytes)?;
            self.stale = false;
        } else if let Err(error) = self.storage.append(&added) {
            // Part of the entries may be on disk; appending after them
            // would bury them where they read as damage.
            self.stale = true;
            return Err(error.into());
        }
        for (subject, key) in made {
            self.keys.insert(subject.to_vec(), key);
            self.by_fingerprint
                .insert(fingerprint(&key), subject.to_vec());
        }
        Ok(())
    }

    /// The storage underneath, for tests to count its writes.
    #[doc(hidden)]
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Destroys the subject's key. Returns whether there was one. The key
    /// is gone from memory even if the file cannot be written: the caller
    /// records an erasure before asking for it, and a value sealed under the
    /// key afterwards would be destroyed with it when the erasure is
    /// finished. The next write replaces the file.
    pub fn erase(&mut self, subject: &[u8]) -> Result<bool, KeyError> {
        let Some(key) = self.keys.remove(subject) else {
            return Ok(false);
        };
        self.by_fingerprint.remove(&fingerprint(&key));
        self.stale = true;
        let bytes = self.entries()?;
        self.storage.replace(&bytes)?;
        self.stale = false;
        Ok(true)
    }

    /// Every key in memory as the file's bytes: the header, then the keys
    /// in subject order.
    fn entries(&self) -> Result<Vec<u8>, KeyError> {
        let mut subjects: Vec<&Vec<u8>> = self.keys.keys().collect();
        subjects.sort();
        let mut bytes = header().to_vec();
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

/// The keys of a file, and whether the file is whole: a header and whole
/// entries, nothing else. A file that is not gets replaced by the next
/// write, so one with no header yet (none was ever written, or a crash cut
/// its first write short) gets its header then.
fn read_file(bytes: &[u8]) -> Result<(HashMap<Vec<u8>, Key>, bool), KeyError> {
    let Some(entries) = bytes.strip_prefix(&header()[..]) else {
        if let Some(version) = later_version(bytes) {
            return Err(KeyError::NewerFormat { version });
        }
        // Keys behind a header that does not check out, or behind none
        // (a file from before key files had one), may be in use.
        if (0..bytes.len()).any(|from| whole_entry(bytes, from).is_some()) {
            return Err(KeyError::Damaged);
        }
        return Ok((HashMap::new(), false));
    };
    let (keys, valid) = parse(entries)?;
    Ok((keys, valid == entries.len()))
}

/// The layout's number, if the file says it is a key file of a later one.
fn later_version(bytes: &[u8]) -> Option<u32> {
    let rest = bytes.strip_prefix(&MAGIC[..])?;
    let version = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    (version > VERSION).then_some(version)
}

/// The keys, and how many bytes from the start are whole entries.
fn parse(bytes: &[u8]) -> Result<(HashMap<Vec<u8>, Key>, usize), KeyError> {
    let mut keys = HashMap::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(end) = whole_entry(bytes, at) else {
            // A torn append, unless a whole key follows: then this is damage
            // in the middle, and the keys after it must not be cut.
            if (at + 1..bytes.len()).any(|from| whole_entry(bytes, from).is_some()) {
                return Err(KeyError::Damaged);
            }
            break;
        };
        let body = &bytes[at..end - CRC_BYTES];
        let subject = &body[LEN_BYTES..body.len() - KEY_LEN];
        let key: Key = body[body.len() - KEY_LEN..]
            .try_into()
            .expect("KEY_LEN bytes");
        keys.insert(subject.to_vec(), key);
        at = end;
    }
    Ok((keys, at))
}

/// Where the entry starting at `at` ends, if it is all there, its subject
/// is no longer than the limit, and its checksum matches.
fn whole_entry(bytes: &[u8], at: usize) -> Option<usize> {
    let len_bytes = bytes.get(at..at + LEN_BYTES)?;
    let subject_len = usize::from(u16::from_le_bytes([len_bytes[0], len_bytes[1]]));
    if subject_len > MAX_SUBJECT_LEN {
        return None;
    }
    let end = at + LEN_BYTES + subject_len + KEY_LEN + CRC_BYTES;
    let entry = bytes.get(at..end)?;
    let (body, crc) = entry.split_at(entry.len() - CRC_BYTES);
    let crc = u32::from_le_bytes(crc.try_into().expect("4 bytes"));
    (crc32fast::hash(body) == crc).then_some(end)
}
