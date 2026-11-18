//! A backup is the log and the keys as they were at one moment, and a
//! store restored from it holds what the store held then: the same
//! commits under the same ids, and no key that had been destroyed. A
//! backup that was interrupted, at whatever step, still restores to what
//! the backup before it held. The data is synthetic.

use std::io;
use std::path::{Path, PathBuf};

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::backup::{BackupError, DirSource, DirTarget, Source, Target, restore};
use umbrachyx::store::{CommitId, MAIN_BRANCH, Provenance, Store, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-backup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.log"))
    }

    fn keys(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.log.keys"))
    }

    fn open(&self, name: &str) -> FileStore {
        let storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(&self.keys(name)));
        let keys = Keys::open(storage).expect("opens the key file");
        Store::open_sealed(
            FileStorage::open(&self.log(name)).expect("opens file"),
            keys,
            SystemClock,
        )
        .expect("opens")
    }

    fn backup_dir(&self) -> PathBuf {
        self.0.join("backup")
    }

    fn target(&self) -> DirTarget {
        DirTarget::open(&self.backup_dir()).expect("opens the backup")
    }

    /// Restores the backup as the store `name` and opens it.
    fn restored(&self, name: &str) -> FileStore {
        self.restore(name).expect("restores");
        self.open(name)
    }

    fn restore(&self, name: &str) -> Result<u64, BackupError> {
        let mut log = FileStorage::open(&self.log(name)).expect("opens file");
        let mut keys = FileKeyStorage::new(&self.keys(name));
        let mut source = DirSource::new(&self.backup_dir());
        restore(&mut source, &mut log, Some(&mut keys)).map(|restored| restored.records)
    }

    fn backup_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.backup_dir())
            .expect("lists the backup")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("name")
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

fn by(who: &str, why: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
        why: why.to_owned(),
        approved_by: None,
    }
}

fn commit(store: &mut FileStore, key: &str, value: &str) -> CommitId {
    store
        .commit(MAIN_BRANCH, vec![set(key, value)], by("seed", "test data"))
        .expect("commits")
}

fn value(store: &FileStore, key: &str) -> Option<Vec<u8>> {
    store
        .get(MAIN_BRANCH, key.as_bytes())
        .map(|value| value.to_vec())
}

fn assert_same(restored: &FileStore, source: &FileStore) {
    assert_eq!(restored.head(MAIN_BRANCH), source.head(MAIN_BRANCH));
    assert_eq!(restored.entries(MAIN_BRANCH), source.entries(MAIN_BRANCH));
    let ids = |store: &FileStore| -> Vec<_> {
        store.log(MAIN_BRANCH).iter().map(|info| info.id).collect()
    };
    assert_eq!(ids(restored), ids(source));
    assert_eq!(restored.erasures(), source.erasures());
    assert_eq!(restored.log_records(), source.log_records());
}

#[test]
fn a_store_restored_from_a_backup_holds_what_the_store_held() {
    let dir = TempDir::new("same");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    commit(&mut source, "player/2/gems", "7");

    let backup = source.backup(&mut dir.target()).expect("backs up");

    assert_eq!(backup.records, 2);
    assert_eq!(backup.added, 2);
    let restored = dir.restored("restored");
    assert_same(&restored, &source);
    assert_eq!(value(&restored, "player/1/gems"), Some(b"100".to_vec()));
}

#[test]
fn a_second_backup_adds_only_what_was_written_since() {
    let dir = TempDir::new("adds");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    let log_before = std::fs::read(dir.backup_dir().join("log")).expect("reads");
    commit(&mut source, "player/1/gems", "150");
    commit(&mut source, "player/3/gems", "1");

    let backup = source.backup(&mut dir.target()).expect("backs up again");

    assert_eq!((backup.records, backup.added), (3, 2));
    let log_after = std::fs::read(dir.backup_dir().join("log")).expect("reads");
    assert_eq!(&log_after[..log_before.len()], &log_before[..]);
    assert_same(&dir.restored("restored"), &source);
}

#[test]
fn a_backup_with_nothing_new_changes_nothing() {
    let dir = TempDir::new("nothing-new");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");

    let backup = source.backup(&mut dir.target()).expect("backs up again");

    assert_eq!((backup.records, backup.added), (1, 0));
    assert_same(&dir.restored("restored"), &source);
}

#[test]
fn an_erasure_reaches_the_backup_with_the_next_one() {
    let dir = TempDir::new("erasure");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    commit(&mut source, "player/2/gems", "7");
    source.backup(&mut dir.target()).expect("backs up");
    assert_eq!(dir.backup_files(), ["keys-2", "log", "manifest"]);
    let key_of_player_1 = Keys::open(FileKeyStorage::new(&dir.keys("source")))
        .expect("opens the key file")
        .get(b"player/1")
        .copied()
        .expect("has a key");
    source
        .erase(b"player/1", by("ops", "asked to be forgotten"))
        .expect("erases");

    source.backup(&mut dir.target()).expect("backs up again");

    // The key file of the backup before is gone, and no file of the
    // backup holds the key.
    assert_eq!(dir.backup_files(), ["keys-3", "log", "manifest"]);
    for name in dir.backup_files() {
        let bytes = std::fs::read(dir.backup_dir().join(name)).expect("reads");
        assert!(
            !bytes
                .windows(key_of_player_1.len())
                .any(|w| w == key_of_player_1)
        );
    }
    let restored = dir.restored("restored");
    assert_eq!(value(&restored, "player/1/gems"), None);
    assert_eq!(value(&restored, "player/2/gems"), Some(b"7".to_vec()));
    assert_same(&restored, &source);
}

#[test]
fn a_subject_written_again_after_its_erasure_is_restored_with_its_new_key() {
    let dir = TempDir::new("again");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    source
        .erase(b"player/1", by("ops", "asked to be forgotten"))
        .expect("erases");
    commit(&mut source, "player/1/name", "new");

    source.backup(&mut dir.target()).expect("backs up again");

    let restored = dir.restored("restored");
    assert_eq!(value(&restored, "player/1/gems"), None);
    assert_eq!(value(&restored, "player/1/name"), Some(b"new".to_vec()));
}

#[test]
fn a_store_without_keys_is_backed_up_without_a_key_file() {
    let dir = TempDir::new("no-keys");
    let open = |name: &str| {
        Store::open(
            FileStorage::open(&dir.log(name)).expect("opens file"),
            SystemClock,
        )
        .expect("opens")
    };
    let mut source = open("source");
    commit(&mut source, "player/1/gems", "100");

    source.backup(&mut dir.target()).expect("backs up");

    assert_eq!(dir.backup_files(), ["log", "manifest"]);
    let mut log = FileStorage::open(&dir.log("restored")).expect("opens file");
    restore(&mut DirSource::new(&dir.backup_dir()), &mut log, None).expect("restores");
    drop(log);
    assert_eq!(
        value(&open("restored"), "player/1/gems"),
        Some(b"100".to_vec())
    );
}

#[test]
fn a_backup_of_another_store_is_refused_and_left_as_it_was() {
    let dir = TempDir::new("another");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    let mut other = dir.open("other");
    commit(&mut other, "player/1/gems", "999");
    commit(&mut other, "player/2/gems", "999");

    let refused = other.backup(&mut dir.target());

    assert!(
        matches!(refused, Err(BackupError::AnotherLog)),
        "{refused:?}"
    );
    assert_same(&dir.restored("restored"), &source);
}

#[test]
fn a_backup_ahead_of_the_store_is_refused() {
    // A store restored from an older backup and written to since has
    // another history than a newer backup of the store it came from.
    let dir = TempDir::new("ahead");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    let mut restored = dir.restored("restored");
    commit(&mut source, "player/1/gems", "150");
    source.backup(&mut dir.target()).expect("backs up again");
    commit(&mut restored, "player/1/gems", "3");
    commit(&mut restored, "player/1/gems", "4");

    let refused = restored.backup(&mut dir.target());

    assert!(
        matches!(refused, Err(BackupError::AnotherLog)),
        "{refused:?}"
    );
}

#[test]
fn restoring_where_there_is_no_backup_is_refused() {
    let dir = TempDir::new("none");

    let refused = dir.restore("restored");

    assert!(matches!(refused, Err(BackupError::NoBackup)), "{refused:?}");
}

#[test]
fn restoring_over_a_store_is_refused() {
    let dir = TempDir::new("over");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    let mut existing = dir.open("existing");
    commit(&mut existing, "player/9/gems", "5");
    drop(existing);

    let refused = dir.restore("existing");

    assert!(matches!(refused, Err(BackupError::NotEmpty)), "{refused:?}");
    assert_eq!(
        value(&dir.open("existing"), "player/9/gems"),
        Some(b"5".to_vec())
    );
}

#[test]
fn a_damaged_backup_is_refused() {
    let dir = TempDir::new("damaged");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    commit(&mut source, "player/2/gems", "7");
    source.backup(&mut dir.target()).expect("backs up");
    let path = dir.backup_dir().join("log");
    let mut bytes = std::fs::read(&path).expect("reads");
    let last = bytes.len() - 3;
    bytes[last] ^= 0x40;
    std::fs::write(&path, bytes).expect("writes");

    let refused = dir.restore("restored");

    assert!(
        matches!(refused, Err(BackupError::Damaged(_) | BackupError::Log(_))),
        "{refused:?}"
    );
}

/// Changes one file of the backup and says how restoring it went. Where
/// the store was to be, nothing may be left.
fn restored_after(name: &str, file: &str, change: impl FnOnce(&mut Vec<u8>)) -> BackupError {
    let dir = TempDir::new(name);
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    commit(&mut source, "player/2/gems", "7");
    source.backup(&mut dir.target()).expect("backs up");
    let path = dir.backup_dir().join(file);
    let mut bytes = std::fs::read(&path).expect("reads");
    change(&mut bytes);
    std::fs::write(&path, bytes).expect("writes");

    let refused = dir.restore("restored").expect_err("is refused");

    assert_eq!(std::fs::read(dir.log("restored")).expect("reads"), b"");
    assert!(!dir.keys("restored").exists());
    refused
}

#[test]
fn a_key_file_that_is_not_the_manifests_is_refused_before_anything_is_written() {
    let cut = restored_after("keys-cut", "keys-2", |bytes| {
        bytes.truncate(bytes.len() - 1)
    });
    let flipped = restored_after("keys-flipped", "keys-2", |bytes| bytes[40] ^= 1);
    let emptied = restored_after("keys-emptied", "keys-2", Vec::clear);

    for refused in [cut, flipped, emptied] {
        assert!(matches!(refused, BackupError::Damaged(_)), "{refused:?}");
    }
}

#[test]
fn a_manifest_that_does_not_check_out_is_refused() {
    let flipped = restored_after("manifest-flipped", "manifest", |bytes| bytes[30] ^= 1);
    let cut = restored_after("manifest-cut", "manifest", |bytes| bytes.truncate(50));
    let later = restored_after("manifest-later", "manifest", |bytes| bytes[8] = 9);

    assert!(matches!(flipped, BackupError::Damaged(_)), "{flipped:?}");
    assert!(matches!(cut, BackupError::Damaged(_)), "{cut:?}");
    assert!(
        matches!(later, BackupError::NewerFormat { version: 9 }),
        "{later:?}"
    );
}

#[test]
fn restoring_changes_nothing_in_the_backup_and_makes_nothing_where_there_is_none() {
    let dir = TempDir::new("read-only");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    let files = |dir: &TempDir| -> Vec<(String, Vec<u8>)> {
        dir.backup_files()
            .into_iter()
            .map(|name| {
                let bytes = std::fs::read(dir.backup_dir().join(&name)).expect("reads");
                (name, bytes)
            })
            .collect()
    };
    let before = files(&dir);
    let nowhere = dir.0.join("nowhere");

    dir.restore("restored").expect("restores");
    let mut log = FileStorage::open(&dir.log("other")).expect("opens file");
    let refused = restore(&mut DirSource::new(&nowhere), &mut log, None);

    assert_eq!(files(&dir), before);
    assert!(matches!(refused, Err(BackupError::NoBackup)), "{refused:?}");
    assert!(!nowhere.exists());
}

#[test]
fn a_file_named_log_that_is_not_a_backups_is_not_written_over() {
    let dir = TempDir::new("their-log");
    std::fs::create_dir_all(dir.backup_dir()).expect("creates");
    std::fs::write(dir.backup_dir().join("log"), b"their own notes").expect("writes");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");

    let refused = source.backup(&mut dir.target());

    assert!(
        matches!(refused, Err(BackupError::NotABackup)),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read(dir.backup_dir().join("log")).expect("reads"),
        b"their own notes"
    );
}

#[test]
fn a_key_made_since_the_last_backup_with_no_record_yet_changes_nothing_in_it() {
    let dir = TempDir::new("same-records");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    source.backup(&mut dir.target()).expect("backs up");
    let key_file = std::fs::read(dir.backup_dir().join("keys-1")).expect("reads");
    drop(source);
    // A key with nothing sealed under it in the log, as a write that was
    // stopped before its record leaves one.
    Keys::open(FileKeyStorage::new(&dir.keys("source")))
        .expect("opens the key file")
        .key_for(b"player/2")
        .expect("makes a key");
    let mut source = dir.open("source");

    let backup = source.backup(&mut dir.target()).expect("backs up again");

    assert_eq!(backup.added, 0);
    assert_eq!(
        std::fs::read(dir.backup_dir().join("keys-1")).expect("reads"),
        key_file
    );
    assert_same(&dir.restored("restored"), &source);
}

#[test]
fn a_directory_that_holds_other_files_is_not_taken_for_a_backup() {
    let dir = TempDir::new("foreign");
    std::fs::create_dir_all(dir.backup_dir()).expect("creates");
    std::fs::write(dir.backup_dir().join("notes.txt"), b"mine").expect("writes");

    let refused = DirTarget::open(&dir.backup_dir());

    assert!(refused.is_err());
    assert_eq!(dir.backup_files(), ["notes.txt"]);
}

/// A backup whose writes stop after so many, as when the process is
/// killed: what was written stays, and nothing more is.
struct Stopping {
    target: DirTarget,
    writes_left: usize,
}

impl Stopping {
    fn write(&mut self) -> io::Result<&mut DirTarget> {
        if self.writes_left == 0 {
            return Err(io::Error::other("killed"));
        }
        self.writes_left -= 1;
        Ok(&mut self.target)
    }
}

impl Source for Stopping {
    fn log_len(&mut self) -> io::Result<u64> {
        self.target.log_len()
    }
    fn log_read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.target.log_read(offset, len)
    }
    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        self.target.read(name)
    }
}

impl Target for Stopping {
    fn log_truncate(&mut self, len: u64) -> io::Result<()> {
        self.write()?.log_truncate(len)
    }
    fn log_append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write()?.log_append(bytes)
    }
    fn log_sync(&mut self) -> io::Result<()> {
        self.write()?.log_sync()
    }
    fn replace(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.write()?.replace(name, bytes)
    }
    fn remove(&mut self, name: &str) -> io::Result<()> {
        self.write()?.remove(name)
    }
    fn names(&mut self) -> io::Result<Vec<String>> {
        self.target.names()
    }
}

/// A store with a backup, then an erasure, a key for a new subject and
/// more writes that the backup does not hold yet.
fn written_past_its_backup(dir: &TempDir) -> (FileStore, CommitId) {
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    let backed_up = commit(&mut source, "player/2/gems", "7");
    source.backup(&mut dir.target()).expect("backs up");
    source
        .erase(b"player/1", by("ops", "asked to be forgotten"))
        .expect("erases");
    commit(&mut source, "player/3/gems", "1");
    commit(&mut source, "player/1/name", "new");
    (source, backed_up)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("creates");
    for entry in std::fs::read_dir(from).expect("lists") {
        let entry = entry.expect("entry");
        std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copies");
    }
}

#[test]
fn a_backup_stopped_at_any_write_still_restores_and_the_next_one_finishes_it() {
    let mut writes = 0;
    loop {
        let dir = TempDir::new(&format!("stopped-{writes}"));
        let (mut source, backed_up) = written_past_its_backup(&dir);
        let mut stopping = Stopping {
            target: dir.target(),
            writes_left: writes,
        };
        let stopped = source.backup(&mut stopping);
        drop(stopping);

        // Whatever was written, the backup restores: to what the backup
        // before held, or to what the store holds now.
        let before = dir.restored("stopped");
        if before.head(MAIN_BRANCH) == Some(backed_up) {
            assert_eq!(value(&before, "player/1/gems"), Some(b"100".to_vec()));
            assert_eq!(value(&before, "player/3/gems"), None);
        } else {
            assert_same(&before, &source);
            assert_eq!(value(&before, "player/1/gems"), None);
        }
        drop(before);

        // And a backup that runs to its end holds the store as it is.
        source.backup(&mut dir.target()).expect("backs up");
        let restored = dir.restored("finished");
        assert_same(&restored, &source);
        assert_eq!(value(&restored, "player/1/gems"), None);
        assert_eq!(value(&restored, "player/1/name"), Some(b"new".to_vec()));
        assert_eq!(dir.backup_files(), ["keys-5", "log", "manifest"]);

        if stopped.is_ok() {
            break;
        }
        writes += 1;
        assert!(writes < 100, "the backup never ran to its end");
    }
    assert!(writes > 3, "stopped at {writes} writes only");
}

#[test]
fn a_first_backup_stopped_part_way_is_no_backup_and_the_next_one_starts_over() {
    let dir = TempDir::new("first");
    let mut source = dir.open("source");
    commit(&mut source, "player/1/gems", "100");
    let mut stopping = Stopping {
        target: dir.target(),
        writes_left: 2,
    };
    source.backup(&mut stopping).expect_err("is stopped");
    drop(stopping);

    let refused = dir.restore("early");
    assert!(matches!(refused, Err(BackupError::NoBackup)), "{refused:?}");

    source.backup(&mut dir.target()).expect("backs up");
    assert_same(&dir.restored("restored"), &source);
}

#[test]
fn a_copy_of_a_backup_restores_as_the_backup_does() {
    let dir = TempDir::new("copy");
    let (mut source, _) = written_past_its_backup(&dir);
    source.backup(&mut dir.target()).expect("backs up");
    let copy = dir.0.join("copy");
    copy_dir(&dir.backup_dir(), &copy);

    let mut log = FileStorage::open(&dir.log("restored")).expect("opens file");
    let mut keys = FileKeyStorage::new(&dir.keys("restored"));
    restore(&mut DirSource::new(&copy), &mut log, Some(&mut keys)).expect("restores");
    drop(log);

    assert_same(&dir.open("restored"), &source);
}
