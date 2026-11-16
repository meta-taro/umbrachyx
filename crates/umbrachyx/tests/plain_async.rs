//! The plain-async engine: a table declared `PlainAsync` is a plain table
//! whose writes, sent through a committer, are answered once they are in
//! memory and reach the disk shortly after. A power cut may lose the last
//! of them; it loses nothing else, and never one answered before a write
//! that survived.
//!
//! "The disk as a power cut would leave it" is a copy of the log file,
//! taken while the store still holds it open.
//!
//! The data is synthetic: cached leaderboards and player gold.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-plain-async-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self) -> PathBuf {
        self.0.join("log")
    }

    /// The store a power cut at this moment would leave: what the log
    /// file holds now, opened on its own.
    fn after_a_power_cut(&self) -> FileStore {
        let copy = self.0.join("copy");
        std::fs::copy(self.log(), &copy).expect("copies the log");
        open(&copy)
    }

    fn log_len(&self) -> u64 {
        std::fs::metadata(self.log())
            .expect("the log is there")
            .len()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(path: &Path) -> FileStore {
    Store::open(FileStorage::open(path).expect("opens file"), SystemClock).expect("opens store")
}

fn by(who: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
        why: "test".to_owned(),
        approved_by: None,
    }
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

fn value(store: &FileStore, key: &str) -> Option<String> {
    store
        .get(MAIN_BRANCH, key.as_bytes())
        .map(|bytes| String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

/// `cache` answers before the disk, `session` waits for it, `player`
/// keeps its history.
fn store(dir: &TempDir) -> FileStore {
    let mut store = open(&dir.log());
    store
        .declare_table(b"cache", Engine::PlainAsync, by("owner"))
        .expect("declares");
    store
        .declare_table(b"session", Engine::Plain, by("owner"))
        .expect("declares");
    store
}

#[test]
fn a_put_is_answered_and_read_before_it_is_on_disk() {
    let dir = TempDir::new("answered");
    let committer = Committer::start(store(&dir));
    let (writer, reader) = (committer.writer(), committer.reader());
    let len = dir.log_len();

    writer
        .put(vec![set("cache/top", "1")])
        .wait()
        .expect("puts");

    assert_eq!(
        reader.view().get(MAIN_BRANCH, b"cache/top").as_deref(),
        Some(&b"1"[..]),
        "read as soon as it is answered"
    );
    assert!(writer.has_unsynced_puts());
    assert_eq!(dir.log_len(), len, "nothing written yet");
    assert_eq!(value(&dir.after_a_power_cut(), "cache/top"), None);

    writer.sync_puts().expect("syncs");

    assert!(!writer.has_unsynced_puts());
    assert_eq!(
        value(&dir.after_a_power_cut(), "cache/top"),
        Some("1".to_owned())
    );
}

#[test]
fn stopping_writes_every_put_that_was_answered() {
    let dir = TempDir::new("stop");
    let committer = Committer::start(store(&dir));
    let writer = committer.writer();
    for n in 0..10 {
        writer
            .put(vec![set("cache/top", &n.to_string())])
            .wait()
            .expect("puts");
    }

    drop(committer.stop());

    assert_eq!(value(&open(&dir.log()), "cache/top"), Some("9".to_owned()));
}

/// Whatever must be on disk before its answer takes the puts answered
/// earlier with it, in the same record: a write that survives a power
/// cut never has an answered write before it missing.
#[test]
fn a_write_that_waits_for_the_disk_carries_the_earlier_puts() {
    for (name, waits) in [("commit", "player/1/gold"), ("plain", "session/1/token")] {
        let dir = TempDir::new(name);
        let store = store(&dir);
        let records = store.log_records();
        let committer = Committer::start(store);
        let writer = committer.writer();
        writer.put(vec![set("cache/a", "1")]).wait().expect("puts");
        writer.put(vec![set("cache/a", "2")]).wait().expect("puts");

        if waits.starts_with("player") {
            writer
                .commit(MAIN_BRANCH, vec![set(waits, "5")], by("game"))
                .expect("commits");
        } else {
            writer.put(vec![set(waits, "t")]).wait().expect("puts");
        }

        assert!(!writer.has_unsynced_puts(), "{name}");
        let left = dir.after_a_power_cut();
        assert_eq!(value(&left, "cache/a"), Some("2".to_owned()), "{name}");
        assert!(value(&left, waits).is_some(), "{name}");
        assert_eq!(
            left.log_records(),
            records + 1,
            "{name}: one record, so one sync"
        );
    }
}

/// The puts held in memory are bounded: past the bound they are written
/// without being asked.
#[test]
fn held_puts_are_written_once_they_pass_the_bound() {
    let dir = TempDir::new("bound");
    let committer = Committer::start(store(&dir));
    let writer = committer.writer();
    let len = dir.log_len();
    let megabyte = "x".repeat(1024 * 1024);

    for n in 0..6 {
        writer
            .put(vec![set(&format!("cache/{n}"), &megabyte)])
            .wait()
            .expect("puts");
    }

    assert!(dir.log_len() > len + 4 * 1024 * 1024);
    drop(committer.stop());
    let store = open(&dir.log());
    for n in 0..6 {
        assert!(value(&store, &format!("cache/{n}")).is_some());
    }
}

/// Only the committer answers early. A plain table through the committer,
/// and a plain-async table written on the store itself, are on disk when
/// answered.
#[test]
fn everything_else_is_on_disk_when_answered() {
    let dir = TempDir::new("durable");
    let mut store = store(&dir);
    store.put(vec![set("cache/direct", "1")]).expect("puts");
    assert_eq!(
        value(&dir.after_a_power_cut(), "cache/direct"),
        Some("1".to_owned())
    );
    let committer = Committer::start(store);
    let writer = committer.writer();

    writer
        .put(vec![set("session/1/token", "t")])
        .wait()
        .expect("puts");
    writer
        .put(vec![set("session/2/token", "t"), set("cache/mixed", "1")])
        .wait()
        .expect("puts");

    let left = dir.after_a_power_cut();
    assert!(value(&left, "session/1/token").is_some());
    assert!(value(&left, "session/2/token").is_some());
    assert!(value(&left, "cache/mixed").is_some());
    assert!(!writer.has_unsynced_puts());
}
