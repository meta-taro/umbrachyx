//! A backup: a copy of the log and of the keys as they were at one moment,
//! kept somewhere else, from which a store can be made again.
//!
//! The log and the key file are two files, and they only make sense
//! together: a log whose values are sealed under a key the key file
//! lacks does not open, and a key file that still holds a key the log
//! says was destroyed has not forgotten what it was told to. Copying one
//! and then the other lets a write come between them. So a backup is
//! taken by the store itself, between two writes, and says in a third
//! file which copy of the keys goes with how much of the log.
//!
//! A backup is three kinds of file:
//!
//! - `log`, the bytes of the store's log from its start. Only records
//!   that were synced are copied.
//! - `keys-N`, every key the store held when its log had `N` records.
//!   Absent when the store has no keys.
//! - `manifest`, written last: how many records the backup holds, how
//!   many bytes of `log` they are, the chain over them and the log's
//!   name.
//!
//! Backing up to the same place again adds the records written since to
//! `log`, writes the keys as they are now beside the older ones, replaces
//! the manifest, and only then removes the older key file. Stopped at any
//! of these steps, the manifest still names a log and a key file that are
//! both there and go together: the backup is what it was before, or what
//! the store is now, never a mix. A subject erased since the last backup
//! has no key in the new key file, and the old one is removed, so an
//! erasure reaches a backup with the next one taken.
//!
//! That is as far as it reaches. A copy made of an older backup still
//! holds the keys of its day, and restoring it brings back what was
//! erased after it. Copies of a backup have to be thrown away on a
//! schedule for an erasure to be final.
//!
//! Restoring writes a new log and key file from the backup and checks
//! every record on the way. It does not open the backup as a store, and
//! refuses to write over one.

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{Clock, Store};
use crate::keys::{KeyError, KeyStorage};
use crate::log::{self, Chain, HEADER_LEN, LogError, LogId, Storage, sync_parent_dir};

const MANIFEST: &str = "manifest";
const KEYS_PREFIX: &str = "keys-";
const LOG: &str = "log";

/// The log is copied this many bytes at a time.
const COPY_CHUNK: usize = 1024 * 1024;

/// Where a backup is kept. The simulation swaps the directory for one
/// that loses and tears writes.
pub trait Target {
    /// How many bytes the copy of the log holds, synced or not.
    fn log_len(&mut self) -> io::Result<u64>;
    /// `len` bytes of the copy of the log from `offset` on.
    fn log_read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// Cuts the copy of the log down to `len` bytes.
    fn log_truncate(&mut self, len: u64) -> io::Result<()>;
    /// Adds bytes at the end of the copy of the log. They are not durable
    /// until [`Target::log_sync`] returns.
    fn log_append(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn log_sync(&mut self) -> io::Result<()>;
    /// The whole of another file of the backup, or `None` if there is
    /// none by that name.
    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>>;
    /// Makes `name` hold `bytes`, durable when this returns. A crash
    /// leaves the file as it was or as it is to be, never a mix.
    fn replace(&mut self, name: &str, bytes: &[u8]) -> io::Result<()>;
    /// Removes `name`; nothing happens if there is none.
    fn remove(&mut self, name: &str) -> io::Result<()>;
    /// The names of the backup's files other than the copy of the log.
    fn names(&mut self) -> io::Result<Vec<String>>;
}

#[derive(Debug)]
pub enum BackupError {
    Io(io::Error),
    Log(LogError),
    Keys(KeyError),
    /// There is no finished backup here.
    NoBackup,
    /// The backup's files do not go together as its manifest says.
    Damaged(&'static str),
    /// The backup is of another log, or of this one with another
    /// history: adding to it would mix the two.
    AnotherLog,
    /// Where the store was to be restored there is one already.
    NotEmpty,
    /// The backup has keys and nowhere was given to put them.
    NeedsKeys,
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "backup storage failed: {error}"),
            Self::Log(error) => write!(f, "{error}"),
            Self::Keys(error) => write!(f, "{error}"),
            Self::NoBackup => write!(f, "there is no finished backup here"),
            Self::Damaged(reason) => write!(f, "the backup is damaged: {reason}"),
            Self::AnotherLog => write!(
                f,
                "the backup holds another log, or another history of this one"
            ),
            Self::NotEmpty => write!(f, "there is a store here already; restore to a new place"),
            Self::NeedsKeys => write!(f, "the backup has keys and nowhere to put them was given"),
        }
    }
}

impl std::error::Error for BackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Log(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for BackupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LogError> for BackupError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

impl From<KeyError> for BackupError {
    fn from(error: KeyError) -> Self {
        Self::Keys(error)
    }
}

/// What a backup that ran to its end did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backup {
    /// Records the backup holds now.
    pub records: u64,
    /// How many of them this backup added.
    pub added: u64,
    /// Bytes of the log this backup copied.
    pub bytes: u64,
}

/// What restoring wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    pub records: u64,
}

/// What a finished backup holds: `magic: 8 | version: u32 LE |
/// records: u64 LE | bytes of log: u64 LE | chain: 32 | log id: 16 |
/// has keys: 1 | crc32: u32 LE`, the checksum over all that comes before
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Manifest {
    records: u64,
    /// Bytes of the copy of the log that hold those records, its header
    /// counted.
    log_len: u64,
    chain: Chain,
    id: LogId,
    has_keys: bool,
}

const MAGIC: [u8; 8] = *b"UMBRABAK";
/// Bumped when the manifest's layout changes.
const VERSION: u32 = 1;
const MANIFEST_LEN: usize = 8 + 4 + 8 + 8 + 32 + 16 + 1 + 4;
const CHECKED_LEN: usize = MANIFEST_LEN - 4;

impl Manifest {
    fn encode(&self) -> [u8; MANIFEST_LEN] {
        let mut bytes = [0; MANIFEST_LEN];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.records.to_le_bytes());
        bytes[20..28].copy_from_slice(&self.log_len.to_le_bytes());
        bytes[28..60].copy_from_slice(&self.chain);
        bytes[60..76].copy_from_slice(&self.id);
        bytes[76] = u8::from(self.has_keys);
        let crc = crc32fast::hash(&bytes[..CHECKED_LEN]);
        bytes[CHECKED_LEN..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, BackupError> {
        let damaged = BackupError::Damaged("the manifest does not check out");
        let Ok(bytes) = <&[u8; MANIFEST_LEN]>::try_from(bytes) else {
            return Err(damaged);
        };
        let crc = u32::from_le_bytes(word(&bytes[CHECKED_LEN..]));
        if bytes[..8] != MAGIC || crc != crc32fast::hash(&bytes[..CHECKED_LEN]) {
            return Err(damaged);
        }
        if u32::from_le_bytes(word(&bytes[8..12])) != VERSION {
            return Err(BackupError::Damaged(
                "the manifest is in a layout this version does not know",
            ));
        }
        Ok(Self {
            records: u64::from_le_bytes(word(&bytes[12..20])),
            log_len: u64::from_le_bytes(word(&bytes[20..28])),
            chain: word(&bytes[28..60]),
            id: word(&bytes[60..76]),
            has_keys: bytes[76] != 0,
        })
    }

    /// The manifest of the backup at `target`, if one was finished there.
    fn read(target: &mut impl Target) -> Result<Option<Self>, BackupError> {
        target
            .read(MANIFEST)?
            .map(|bytes| Self::decode(&bytes))
            .transpose()
    }

    fn keys_name(&self) -> String {
        format!("{KEYS_PREFIX}{}", self.records)
    }
}

/// A slice of exactly `N` bytes as an array. The callers slice fixed
/// ranges of a buffer whose length was checked.
fn word<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut word = [0; N];
    word.copy_from_slice(bytes);
    word
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Brings the backup at `target` up to what this store holds now: the
    /// records its log has synced, and the keys as they are. Nothing can
    /// be written to the store meanwhile, which is what makes the two go
    /// together.
    ///
    /// A backup of another log, or of a history this store does not
    /// have, is refused and left as it was. Stopped part way, the backup
    /// stays what it was before, and running this again finishes it.
    pub fn backup(&mut self, target: &mut impl Target) -> Result<Backup, BackupError> {
        let manifest = Manifest {
            records: self.log.records(),
            log_len: self.log.file_len(),
            chain: self.chain_now()?,
            id: self.log.id(),
            has_keys: self.sealer.is_some(),
        };
        let held = self.start_of_copy(target)?;
        let mut copied = held.map_or(HEADER_LEN as u64, |held| held.log_len);
        let from = copied;
        while copied < manifest.log_len {
            let len = usize::try_from(manifest.log_len - copied)
                .unwrap_or(COPY_CHUNK)
                .min(COPY_CHUNK);
            let bytes = self.log.read_file(copied, len)?;
            target.log_append(&bytes)?;
            copied += bytes.len() as u64;
        }
        target.log_sync()?;
        if let Some(sealer) = &self.sealer {
            target.replace(&manifest.keys_name(), &sealer.key_file()?)?;
        }
        target.replace(MANIFEST, &manifest.encode())?;
        remove_other_keys(target, &manifest)?;
        Ok(Backup {
            records: manifest.records,
            added: manifest.records - held.map_or(0, |held| held.records),
            bytes: copied - from,
        })
    }

    fn chain_now(&mut self) -> Result<Chain, BackupError> {
        let records = self.log.records();
        self.log
            .chain_at(records)?
            .ok_or(BackupError::Damaged("the log lost records it had"))
    }

    /// Makes the copy of the log at `target` end where its last finished
    /// backup ended, and returns that backup's manifest: what the copy
    /// holds, to which this store's later records are added. With no
    /// finished backup there the copy starts over, with the header.
    fn start_of_copy(&mut self, target: &mut impl Target) -> Result<Option<Manifest>, BackupError> {
        let Some(held) = Manifest::read(target)? else {
            target.log_truncate(0)?;
            target.log_append(&self.log.header_bytes())?;
            return Ok(None);
        };
        if held.id != self.log.id() || self.log.chain_at(held.records)? != Some(held.chain) {
            return Err(BackupError::AnotherLog);
        }
        if target.log_len()? < held.log_len {
            return Err(BackupError::Damaged(
                "the copy of the log is shorter than the manifest says",
            ));
        }
        if target.log_read(0, HEADER_LEN)? != self.log.header_bytes() {
            return Err(BackupError::AnotherLog);
        }
        // Past the manifest's end is what a backup that did not finish
        // left behind.
        target.log_truncate(held.log_len)?;
        Ok(Some(held))
    }
}

/// Removes every key file but the manifest's own. This is where a key
/// erased since the backup before leaves the backup.
fn remove_other_keys(target: &mut impl Target, manifest: &Manifest) -> Result<(), BackupError> {
    let kept = manifest.has_keys.then(|| manifest.keys_name());
    for name in target.names()? {
        if name.starts_with(KEYS_PREFIX) && Some(&name) != kept.as_ref() {
            target.remove(&name)?;
        }
    }
    Ok(())
}

/// Writes the store the backup at `target` holds into `log` and `keys`,
/// which must both be empty: a new place, not a store that exists. Every
/// record is checked against the manifest on the way; a backup that does
/// not check out is refused before anything is written.
///
/// The store is then opened as any other is.
pub fn restore(
    target: &mut impl Target,
    log: &mut impl Storage,
    keys: Option<&mut dyn KeyStorage>,
) -> Result<Restored, BackupError> {
    let manifest = Manifest::read(target)?.ok_or(BackupError::NoBackup)?;
    let len = usize::try_from(manifest.log_len)
        .map_err(|_| BackupError::Damaged("the manifest names more bytes than fit in memory"))?;
    if target.log_len()? < manifest.log_len {
        return Err(BackupError::Damaged(
            "the copy of the log is shorter than the manifest says",
        ));
    }
    let bytes = target.log_read(0, len)?;
    let copied = log::check_copy(&bytes)?;
    if (copied.id, copied.records, copied.chain) != (manifest.id, manifest.records, manifest.chain)
    {
        return Err(BackupError::Damaged(
            "the copy of the log is not what the manifest says",
        ));
    }
    let key_file = if manifest.has_keys {
        let bytes = target
            .read(&manifest.keys_name())?
            .ok_or(BackupError::Damaged("the key file is missing"))?;
        Some((keys.ok_or(BackupError::NeedsKeys)?, bytes))
    } else {
        None
    };
    if !log.read_all()?.iter().all(|&byte| byte == 0) {
        return Err(BackupError::NotEmpty);
    }
    // The keys first: a log without its keys does not open, and keys
    // without a log are a store with nothing in it.
    if let Some((keys, bytes)) = key_file {
        if !keys.read_all()?.is_empty() {
            return Err(BackupError::NotEmpty);
        }
        keys.replace(&bytes)?;
    }
    log.truncate(0)?;
    log.append(&bytes)?;
    log.sync()?;
    Ok(Restored {
        records: manifest.records,
    })
}

/// A backup kept in a directory of its own. The copy of the log is held
/// open and locked, so two backups cannot write to one place at once.
#[derive(Debug)]
pub struct DirTarget {
    dir: PathBuf,
    log: File,
}

impl DirTarget {
    /// Opens the backup in `dir`, making the directory if there is none.
    /// A directory that holds files and no manifest is refused unless
    /// all it holds is what a first backup left when it was stopped:
    /// it is somebody's, and a backup would write among their files.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let names = file_names(dir)?;
        let is_ours = |name: &String| name == LOG || is_backup_file(name);
        if !names.iter().any(|name| name == MANIFEST) && !names.iter().all(is_ours) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} holds files that are not a backup's; name an empty directory",
                    dir.display()
                ),
            ));
        }
        let path = dir.join(LOG);
        let log = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        log.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is in use by another backup", dir.display()),
            ),
            TryLockError::Error(error) => error,
        })?;
        sync_parent_dir(&path)?;
        Ok(Self {
            dir: dir.to_owned(),
            log,
        })
    }

    /// A file only its owner can read on Unix: some of them hold keys.
    fn create(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }
}

/// Whether `name` is one of a backup's files beside the copy of the log,
/// whole or still being written.
fn is_backup_file(name: &str) -> bool {
    let name = name.strip_suffix(NEW_SUFFIX).unwrap_or(name);
    name == MANIFEST
        || name
            .strip_prefix(KEYS_PREFIX)
            .is_some_and(|records| records.parse::<u64>().is_ok())
}

/// A file is written under this suffix and renamed into place.
const NEW_SUFFIX: &str = ".new";

fn file_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        // A name that is not Unicode is not one of a backup's.
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

impl Target for DirTarget {
    fn log_len(&mut self) -> io::Result<u64> {
        Ok(self.log.metadata()?.len())
    }

    fn log_read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; len];
        self.log.seek(SeekFrom::Start(offset))?;
        self.log.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn log_truncate(&mut self, len: u64) -> io::Result<()> {
        self.log.set_len(len)
    }

    fn log_append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.log.seek(SeekFrom::End(0))?;
        self.log.write_all(bytes)
    }

    fn log_sync(&mut self) -> io::Result<()> {
        self.log.sync_all()
    }

    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.dir.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn replace(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.dir.join(name);
        let new = self.dir.join(format!("{name}{NEW_SUFFIX}"));
        let mut file = Self::create(&new)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&new, &path)?;
        sync_parent_dir(&path)
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        let path = self.dir.join(name);
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => sync_parent_dir(&path),
        }
    }

    fn names(&mut self) -> io::Result<Vec<String>> {
        Ok(file_names(&self.dir)?
            .into_iter()
            .filter(|name| is_backup_file(name))
            .collect())
    }
}
