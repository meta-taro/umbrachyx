//! Where the log bytes live.
//!
//! The log only talks to [`Storage`], so the deterministic simulation can
//! swap the disk for one that loses unsynced bytes, tears writes and fails
//! syncs on demand. Adding this boundary later would mean rebuilding the log.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub trait Storage {
    /// Everything written so far, synced or not.
    fn read_all(&mut self) -> io::Result<Vec<u8>>;
    /// Add bytes at the end. They are not durable until [`Storage::sync`] returns.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Make every appended byte durable before returning.
    fn sync(&mut self) -> io::Result<()>;
    /// Cut the storage down to `len` bytes.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// A log kept in one file.
///
/// `sync` uses [`File::sync_data`]: `fdatasync` on Linux and
/// `fcntl(F_FULLFSYNC)` on macOS, where a plain `fsync` stops at the drive's
/// cache and does not reach the disk.
#[derive(Debug)]
pub struct FileStorage {
    file: File,
}

impl FileStorage {
    /// Open or create the log file and take an exclusive lock on it, so two
    /// writers cannot interleave frames. The lock is released when the
    /// storage is dropped or the process dies.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is already open by another writer", path.display()),
            ),
            TryLockError::Error(error) => error,
        })?;
        sync_parent_dir(path)?;
        Ok(Self { file })
    }
}

/// A new file is only findable after a crash once its directory entry is
/// durable too. This runs on every open, not only on create: a crash between
/// creating the file and syncing the directory would otherwise never be
/// repaired.
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return File::open(".")?.sync_all();
    };
    File::open(dir)?.sync_all()
}

impl Storage for FileStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        // Appends leave the cursor at the end, so read from the start.
        let mut bytes = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempPath(PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "umbrachyx-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            // Best effort: a leftover file in the temp dir harms nothing.
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn appended_bytes_are_read_back_after_reopening() {
        let path = TempPath::new("reopen");
        let mut storage = FileStorage::open(&path.0).expect("opens");
        storage.append(b"hello ").expect("appends");
        storage.append(b"world").expect("appends");
        storage.sync().expect("syncs");
        drop(storage);

        let mut reopened = FileStorage::open(&path.0).expect("reopens");

        assert_eq!(reopened.read_all().expect("reads"), b"hello world");
    }

    #[test]
    fn a_file_already_open_for_writing_is_refused() {
        let path = TempPath::new("lock");
        let _first = FileStorage::open(&path.0).expect("opens");

        let second = FileStorage::open(&path.0);

        assert!(second.is_err(), "two writers would interleave frames");
    }

    #[test]
    fn appends_after_truncate_continue_from_the_cut() {
        let path = TempPath::new("truncate");
        let mut storage = FileStorage::open(&path.0).expect("opens");
        storage.append(b"keep-torn").expect("appends");
        storage.truncate(4).expect("truncates");
        storage.append(b"!").expect("appends");

        assert_eq!(storage.read_all().expect("reads"), b"keep!");
    }
}
