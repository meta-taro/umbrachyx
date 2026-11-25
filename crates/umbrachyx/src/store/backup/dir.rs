//! A backup kept in a directory of its own: the copy of the log and, beside
//! it, the key files and the manifest.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{BackupError, KEYS_PREFIX, LOG, MANIFEST, Restored, Source, Target, restore};
use crate::keys::{FileKeyStorage, KeyStorage};
use crate::log::{FileStorage, Storage, sync_parent_dir};

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

    /// Opens the backup that is in `dir`, and makes nothing: where there
    /// is no directory, or none with a manifest, it fails. For bringing
    /// up to date a backup known to be there. If the disk it is on has
    /// gone, the same path is another, empty place, and a new backup
    /// begun there would fill a disk nobody chose.
    pub fn open_existing(dir: &Path) -> io::Result<Self> {
        if !dir.join(MANIFEST).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} no longer holds the backup kept there", dir.display()),
            ));
        }
        Self::open(dir)
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

fn read_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_part(file: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl Source for DirTarget {
    fn log_len(&mut self) -> io::Result<u64> {
        Ok(self.log.metadata()?.len())
    }

    fn log_read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        read_part(&mut self.log, offset, len)
    }

    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        read_file(&self.dir.join(name))
    }
}

/// A backup in a directory, to restore from: it is only read, nothing
/// is made or locked there, so it can be a copy that cannot be written
/// to, such as a snapshot.
#[derive(Debug)]
pub struct DirSource {
    dir: PathBuf,
}

impl DirSource {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
        }
    }

    fn log(&self) -> io::Result<Option<File>> {
        match File::open(self.dir.join(LOG)) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Source for DirSource {
    fn log_len(&mut self) -> io::Result<u64> {
        match self.log()? {
            Some(file) => Ok(file.metadata()?.len()),
            None => Ok(0),
        }
    }

    fn log_read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut file = self
            .log()?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        read_part(&mut file, offset, len)
    }

    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        read_file(&self.dir.join(name))
    }
}

impl Target for DirTarget {
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

/// What is added to the name of a store's file while it is being
/// restored.
const RESTORING: &str = ".restoring";

/// `path` with `suffix` added to its name.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.to_owned().into_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Gives the file at `from` the name `to`, which must not be taken: a
/// rename would replace a file made there meanwhile, a link does not.
fn move_into_place(from: &Path, to: &Path) -> Result<(), BackupError> {
    match std::fs::hard_link(from, to) {
        Ok(()) => Ok(std::fs::remove_file(from)?),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(BackupError::NotEmpty),
        Err(error) => Err(error.into()),
    }
}

/// Writes the store the backup at `source` holds to the files `log` and
/// `keys`, neither of which may exist: a new place, not a store.
///
/// The store is written beside its place, under other names, and moved
/// there when it is whole and durable: its log first, its keys last. So
/// a restore that is stopped while it writes, by a signal or a power
/// cut, leaves no file at the place; what it left beside the place is
/// cleared by the next restore. Stopped between the two moves it leaves
/// a log without its keys, which does not open and says so.
///
/// A store made at the place while this one was being written is not
/// replaced: the restore is refused, and nothing of it is left there.
pub fn restore_to(
    source: &mut impl Source,
    log: &Path,
    keys: &Path,
) -> Result<Restored, BackupError> {
    if log.exists() || keys.exists() {
        return Err(BackupError::NotEmpty);
    }
    let (log_beside, keys_beside) = (beside(log, RESTORING), beside(keys, RESTORING));
    // Whoever holds this file writes the two beside the place, and
    // nobody else clears them: a restore that cannot take it leaves them
    // alone. Held until the store is in place, so no other restore
    // empties the file while it is being given its name.
    let mut held = FileStorage::open(&log_beside)?;
    let restored = write_beside(source, &mut held, &keys_beside).and_then(|restored| {
        move_both_into_place(&log_beside, &keys_beside, log, keys)?;
        Ok(restored)
    });
    if restored.is_err() {
        let _ = std::fs::remove_file(&log_beside);
        let _ = std::fs::remove_file(&keys_beside);
    }
    drop(held);
    restored
}

/// Restores into the two files beside the place, whatever an earlier
/// restore left in them, and makes them durable.
fn write_beside(
    source: &mut impl Source,
    log_beside: &mut FileStorage,
    keys_beside: &Path,
) -> Result<Restored, BackupError> {
    log_beside.truncate(0)?;
    match std::fs::remove_file(keys_beside) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let mut keys = FileKeyStorage::new(keys_beside);
    restore(source, log_beside, Some(&mut keys as &mut dyn KeyStorage))
}

/// Gives the two files their names. The log first: it is the file a
/// server makes when it starts, so if one has started at the place this
/// is refused before anything of the restored store is there. Keys moved
/// first would be left in that server's place, for it to add its own to.
fn move_both_into_place(
    log_beside: &Path,
    keys_beside: &Path,
    log: &Path,
    keys: &Path,
) -> Result<(), BackupError> {
    move_into_place(log_beside, log)?;
    if keys_beside.exists()
        && let Err(refused) = move_into_place(keys_beside, keys)
    {
        // A key file that is not this store's: the log just placed is
        // ours and opens with no other keys, so it is taken back.
        let _ = std::fs::remove_file(log);
        return Err(refused);
    }
    sync_parent_dir(log)?;
    sync_parent_dir(keys)?;
    Ok(())
}
