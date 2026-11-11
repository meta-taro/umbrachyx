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
/// `sync` uses [`File::sync_data`]: `fdatasync` on Linux,
/// `fcntl(F_FULLFSYNC)` on macOS, where a plain `fsync` stops at the drive's
/// cache and does not reach the disk, and `FlushFileBuffers` on Windows.
///
/// Opened with [`FileStorage::open_reserving`], the file is zeroed ahead of
/// what is written, a reserve at a time, and records overwrite the zeros.
/// A sync then has only the record's bytes to make durable: the file's
/// length and the blocks it is made of do not change, so the file system
/// has nothing of its own to write, which is most of what a sync of a
/// growing file waits for. The file reads back with the zeros that are
/// left, and whoever opens it says where the records end with
/// [`Storage::truncate`], as the log does.
#[derive(Debug)]
pub struct FileStorage {
    file: File,
    /// Where the next append goes.
    len: u64,
    /// How far the file is zeroed ahead, a multiple of `reserve`; with no
    /// reserve, the file's length.
    reserved: u64,
    /// Bytes zeroed ahead at a time, or zero to let each append grow the
    /// file.
    reserve: u64,
}

/// Zeros are written this many bytes at a time.
const ZERO_CHUNK: usize = 1024 * 1024;

impl FileStorage {
    /// Open or create the log file and take an exclusive lock on it, so two
    /// writers cannot interleave frames. The lock is released when the
    /// storage is dropped or the process dies.
    ///
    /// The file is opened for writing rather than in append mode: on Windows
    /// an append-mode handle cannot be truncated, and recovery needs to cut
    /// a torn tail. The lock makes this the only writer, so seeking to the
    /// end before each append is enough.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_reserving(path, 0)
    }

    /// As [`FileStorage::open`], zeroing the file `reserve` bytes ahead of
    /// what is written. A file that already holds bytes is read as it is;
    /// until `truncate` says where its records end, appends go after all of
    /// it.
    pub fn open_reserving(path: &Path, reserve: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is already open by another writer", path.display()),
            ),
            TryLockError::Error(error) => error,
        })?;
        sync_parent_dir(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            reserved: len,
            reserve,
        })
    }

    /// Zeroes the file from `from` up to `to`.
    fn zero(&mut self, from: u64, to: u64) -> io::Result<()> {
        let zeros = vec![0; ZERO_CHUNK];
        self.file.seek(SeekFrom::Start(from))?;
        let mut left = to.saturating_sub(from);
        while left > 0 {
            let chunk = usize::try_from(left).unwrap_or(ZERO_CHUNK).min(ZERO_CHUNK);
            self.file.write_all(&zeros[..chunk])?;
            left -= chunk as u64;
        }
        Ok(())
    }

    /// Reserves enough for the file to hold `end` bytes. The zeros and the
    /// file's new length are made durable here, once per reserve, so that
    /// no sync of a record has them to wait for.
    fn reserve_up_to(&mut self, end: u64) -> io::Result<()> {
        if self.reserve == 0 || end <= self.reserved {
            return Ok(());
        }
        let reserved = end.next_multiple_of(self.reserve);
        self.zero(self.reserved, reserved)?;
        self.file.sync_all()?;
        self.reserved = reserved;
        Ok(())
    }

    /// The end of the last byte that is not zero from `from` on, or `from`.
    fn end_of_written(&mut self, from: u64) -> io::Result<u64> {
        let mut tail = Vec::new();
        self.file.seek(SeekFrom::Start(from))?;
        self.file.read_to_end(&mut tail)?;
        let written = tail
            .iter()
            .rposition(|&byte| byte != 0)
            .map_or(0, |at| at + 1);
        Ok(from + written as u64)
    }
}

/// A new file is only findable after a crash once its directory entry is
/// durable too. This runs on every open, not only on create: a crash between
/// creating the file and syncing the directory would otherwise never be
/// repaired.
#[cfg(unix)]
pub(crate) fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return File::open(".")?.sync_all();
    };
    File::open(dir)?.sync_all()
}

/// Windows cannot open a directory as a file to flush it. NTFS journals the
/// directory entry as metadata; whether that is enough across a power cut is
/// left to the simulation disk and real-machine tests to show.
#[cfg(not(unix))]
pub(crate) fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

impl Storage for FileStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let end = self.len + bytes.len() as u64;
        self.reserve_up_to(end)?;
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(bytes)?;
        self.len = end;
        self.reserved = self.reserved.max(end);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Without a reserve the file is cut. With one it keeps its length,
    /// and what was written after `len` is zeroed, so the reserve is not
    /// given back and taken again every time the log is opened.
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        if self.reserve == 0 {
            self.file.set_len(len)?;
            self.reserved = len;
        } else {
            let written = self.end_of_written(len)?;
            self.zero(len, written)?;
        }
        self.len = len;
        Ok(())
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

    const RESERVE: u64 = 4_096;

    fn zeros(len: usize) -> Vec<u8> {
        vec![0; len]
    }

    #[test]
    fn a_reserving_file_is_zeroed_ahead_of_what_is_written() {
        let path = TempPath::new("reserve");
        let mut storage = FileStorage::open_reserving(&path.0, RESERVE).expect("opens");
        storage.append(b"hello").expect("appends");
        storage.sync().expect("syncs");

        let bytes = storage.read_all().expect("reads");

        assert_eq!(bytes.len() as u64, RESERVE);
        assert_eq!(&bytes[..5], b"hello");
        assert_eq!(bytes[5..], zeros(RESERVE as usize - 5));
        assert_eq!(std::fs::metadata(&path.0).expect("stats").len(), RESERVE);
    }

    #[test]
    fn appends_past_the_reserve_reserve_more() {
        let path = TempPath::new("reserve-more");
        let mut storage = FileStorage::open_reserving(&path.0, RESERVE).expect("opens");
        let first = vec![1; RESERVE as usize - 3];
        storage.append(&first).expect("appends");
        storage.append(b"across").expect("appends");
        storage.append(b"!").expect("appends");

        let bytes = storage.read_all().expect("reads");

        assert_eq!(bytes.len() as u64, 2 * RESERVE);
        assert_eq!(bytes[..first.len()], first);
        assert_eq!(&bytes[first.len()..first.len() + 7], b"across!");
        assert_eq!(bytes[first.len() + 7..], zeros(RESERVE as usize - 4));
    }

    /// Cutting a torn end off a reserving file zeroes it and keeps the
    /// reserve, and the next append goes where the cut was.
    #[test]
    fn cutting_a_reserving_file_zeroes_the_end_in_place() {
        let path = TempPath::new("reserve-cut");
        let mut storage = FileStorage::open_reserving(&path.0, RESERVE).expect("opens");
        storage.append(b"keep-torn").expect("appends");
        storage.truncate(4).expect("cuts");

        let cut = storage.read_all().expect("reads");
        storage.append(b"!").expect("appends");
        let after = storage.read_all().expect("reads");

        assert_eq!(cut.len() as u64, RESERVE);
        assert_eq!(&cut[..4], b"keep");
        assert_eq!(cut[4..], zeros(RESERVE as usize - 4));
        assert_eq!(&after[..5], b"keep!");
        assert_eq!(after[5..], zeros(RESERVE as usize - 5));
    }

    /// Opened again, the file reads back with its zeros; once told where
    /// the records end, appends go on from there.
    #[test]
    fn a_reserving_file_reopens_and_appends_where_the_records_end() {
        let path = TempPath::new("reserve-reopen");
        let mut storage = FileStorage::open_reserving(&path.0, RESERVE).expect("opens");
        storage.append(b"first").expect("appends");
        storage.sync().expect("syncs");
        drop(storage);

        let mut reopened = FileStorage::open_reserving(&path.0, RESERVE).expect("reopens");
        let read = reopened.read_all().expect("reads");
        reopened.truncate(5).expect("marks the end");
        reopened.append(b"+second").expect("appends");

        assert_eq!(read.len() as u64, RESERVE);
        let bytes = reopened.read_all().expect("reads");
        assert_eq!(&bytes[..12], b"first+second");
        assert_eq!(bytes.len() as u64, RESERVE);
    }

    /// The command-line tool opens the server's log without reserving: the
    /// cut then gives the zeros back, and what was written stays.
    #[test]
    fn a_reserved_file_opened_without_reserving_shrinks_to_its_records() {
        let path = TempPath::new("reserve-plain");
        let mut storage = FileStorage::open_reserving(&path.0, RESERVE).expect("opens");
        storage.append(b"first").expect("appends");
        storage.sync().expect("syncs");
        drop(storage);

        let mut plain = FileStorage::open(&path.0).expect("reopens");
        plain.truncate(5).expect("cuts");
        plain.append(b"+second").expect("appends");

        assert_eq!(plain.read_all().expect("reads"), b"first+second");
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
