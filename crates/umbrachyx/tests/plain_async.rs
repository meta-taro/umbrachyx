//! The plain-async engine: a table declared `PlainAsync` is a plain table
//! whose writes, sent through a committer, are answered once they are in
//! memory and reach the disk shortly after. A power cut may lose the last
//! of them; it loses nothing else, and never one answered before a write
//! that survived.
//!
//! "The disk as a power cut would leave it" is every byte that was
//! appended and then synced, opened as a log of its own.
//!
//! The data is synthetic: cached leaderboards and player gold.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use umbrachyx::log::{Edit, Engine, FileStorage, Storage};
use umbrachyx::store::{Committer, Early, MAIN_BRANCH, Provenance, Store, SystemClock};

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

/// What a power cut would leave of a log: the bytes that were appended
/// and then synced. Kept beside the file because a log held open cannot
/// be read by another handle on every system.
#[derive(Clone, Default)]
struct Disk(Arc<Mutex<Vec<u8>>>);

impl Disk {
    fn len(&self) -> usize {
        self.0.lock().expect("no test panics holding it").len()
    }

    /// The store those bytes open as, in a file of their own.
    fn after_a_power_cut(&self, dir: &TempDir) -> FileStore {
        let copy = dir.0.join("copy");
        let bytes = self.0.lock().expect("no test panics holding it").clone();
        std::fs::write(&copy, bytes).expect("writes the copy");
        let storage = FileStorage::open(&copy).expect("opens file");
        Store::open(storage, SystemClock).expect("opens store")
    }
}

/// A file that tells a [`Disk`] what it has synced.
struct Mirrored {
    file: FileStorage,
    written: Vec<u8>,
    disk: Disk,
}

impl Mirrored {
    fn open(path: &Path) -> (Self, Disk) {
        let mut file = FileStorage::open(path).expect("opens file");
        let written = file.read_all().expect("reads the log");
        let disk = Disk(Arc::new(Mutex::new(written.clone())));
        let mirrored = Self {
            file,
            written,
            disk: disk.clone(),
        };
        (mirrored, disk)
    }
}

impl Storage for Mirrored {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.append(bytes)?;
        self.written.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync()?;
        self.disk
            .0
            .lock()
            .expect("no test panics holding it")
            .clone_from(&self.written);
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.truncate(len)?;
        self.written.truncate(len as usize);
        Ok(())
    }
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

/// The same store, opened so that what is on disk can be asked.
fn mirrored(dir: &TempDir) -> (Store<Mirrored, SystemClock>, Disk) {
    drop(store(dir));
    let (storage, disk) = Mirrored::open(&dir.log());
    let store = Store::open(storage, SystemClock).expect("opens store");
    (store, disk)
}

#[test]
fn a_put_is_answered_and_read_before_it_is_on_disk() {
    let dir = TempDir::new("answered");
    let (store, disk) = mirrored(&dir);
    let committer = Committer::start(store);
    let (writer, reader) = (committer.writer(), committer.reader());
    let len = disk.len();

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
    assert_eq!(disk.len(), len, "nothing written yet");
    assert_eq!(value(&disk.after_a_power_cut(&dir), "cache/top"), None);

    writer.sync_puts().expect("syncs");

    assert!(!writer.has_unsynced_puts());
    assert_eq!(
        value(&disk.after_a_power_cut(&dir), "cache/top"),
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
        let (store, disk) = mirrored(&dir);
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
        let left = disk.after_a_power_cut(&dir);
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
    let (store, disk) = mirrored(&dir);
    let committer = Committer::start(store);
    let writer = committer.writer();
    let len = disk.len();
    let megabyte = "x".repeat(1024 * 1024);

    for n in 0..6 {
        writer
            .put(vec![set(&format!("cache/{n}"), &megabyte)])
            .wait()
            .expect("puts");
    }

    assert!(disk.len() > len + 4 * 1024 * 1024);
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
    let (mut store, disk) = mirrored(&dir);
    store.put(vec![set("cache/direct", "1")]).expect("puts");
    assert_eq!(
        value(&disk.after_a_power_cut(&dir), "cache/direct"),
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

    let left = disk.after_a_power_cut(&dir);
    assert!(value(&left, "session/1/token").is_some());
    assert!(value(&left, "session/2/token").is_some());
    assert!(value(&left, "cache/mixed").is_some());
    assert!(!writer.has_unsynced_puts());
}

/// A thread that serves connections answers the puts it queued itself
/// when none of them waits for the disk, and says when they are the
/// first that somebody must sync in time.
#[test]
fn queued_puts_are_answered_on_the_thread_that_asks_when_none_waits_for_the_disk() {
    let dir = TempDir::new("early");
    let (store, disk) = mirrored(&dir);
    let committer = Committer::start(store);
    let (writer, reader) = (committer.writer(), committer.reader());
    let len = disk.len();
    let mut first = writer.put(vec![set("cache/a", "1")]);
    let mut second = writer.put(vec![set("cache/a", "2"), set("cache/b", "1")]);

    assert_eq!(writer.write_queued_early(), Some(Early::FirstUnsynced));

    assert!(matches!(first.poll(), Some(Ok(()))));
    assert!(matches!(second.poll(), Some(Ok(()))));
    assert_eq!(
        reader.view().get(MAIN_BRANCH, b"cache/a").as_deref(),
        Some(&b"2"[..]),
        "applied in the order sent"
    );
    assert_eq!(disk.len(), len, "nothing written");
    assert!(writer.has_unsynced_puts());

    let mut third = writer.put(vec![set("cache/a", "3")]);
    assert_eq!(writer.write_queued_early(), Some(Early::Written));
    assert!(matches!(third.poll(), Some(Ok(()))));
    assert_eq!(writer.write_queued_early(), None, "nothing queued");

    drop(committer.stop());
    assert_eq!(value(&open(&dir.log()), "cache/a"), Some("3".to_owned()));
}

/// Anything that must be on disk before its answer is left in the queue,
/// with the puts queued around it and in their order, for the thread
/// that may wait for the disk.
#[test]
fn a_queue_with_a_write_that_waits_for_the_disk_is_left_alone() {
    type Send = fn(&umbrachyx::store::Writer);
    let waiting: [(&str, Send); 3] = [
        ("commit", |writer| {
            drop(writer.send(MAIN_BRANCH, vec![set("player/1/gold", "5")], by("game")));
        }),
        ("plain", |writer| {
            drop(writer.put(vec![set("session/1/token", "t")]));
        }),
        ("refused", |writer| {
            drop(writer.put(vec![set("player/1/gold", "5")]));
        }),
    ];
    for (name, send) in waiting {
        let dir = TempDir::new(&format!("left-{name}"));
        let (store, disk) = mirrored(&dir);
        let committer = Committer::start(store);
        let (writer, reader) = (committer.writer(), committer.reader());
        let len = disk.len();
        let mut early = writer.put(vec![set("cache/a", "1")]);
        send(&writer);

        assert_eq!(writer.write_queued_early(), None, "{name}");

        assert!(early.poll().is_none(), "{name}: not answered");
        assert_eq!(reader.view().get(MAIN_BRANCH, b"cache/a"), None, "{name}");
        assert_eq!(disk.len(), len, "{name}: nothing written");

        writer.write_queued();
        assert!(matches!(early.poll(), Some(Ok(()))), "{name}");
    }
}

/// Puts that no longer fit under the bound wait for the disk, so they
/// are not written on the thread that only asks.
#[test]
fn puts_past_the_bound_are_left_for_the_thread_that_waits_for_the_disk() {
    let dir = TempDir::new("early-bound");
    let (store, disk) = mirrored(&dir);
    let committer = Committer::start(store);
    let writer = committer.writer();
    let megabyte = "x".repeat(1024 * 1024);
    for n in 0..3 {
        drop(writer.put(vec![set(&format!("cache/{n}"), &megabyte)]));
    }
    assert!(writer.write_queued_early().is_some());
    let len = disk.len();

    let mut over = writer.put(vec![set("cache/3", &megabyte)]);

    assert_eq!(writer.write_queued_early(), None);
    assert!(over.poll().is_none());
    assert_eq!(disk.len(), len, "nothing written");
    writer.write_queued();
    assert!(matches!(over.poll(), Some(Ok(()))));
    assert!(disk.len() > len + 4 * 1024 * 1024, "all four are on disk");
}

/// Never waits for another thread's turn: while one writes, the queue is
/// left for it.
#[test]
fn nothing_is_answered_early_while_another_thread_writes() {
    let dir = TempDir::new("early-turn");
    let committer = Committer::start(store(&dir));
    let writer = committer.writer();
    let mut early = writer.put(vec![set("cache/a", "1")]);

    let asked = writer
        .ask(|_| writer.write_queued_early())
        .expect("asks the store");

    // The question wrote the queue before it was answered.
    assert_eq!(asked, None);
    assert!(matches!(early.poll(), Some(Ok(()))));
}

/// A disk whose syncs fail once it is told to.
struct FailingStorage {
    file: FileStorage,
    is_failing: Arc<AtomicBool>,
}

impl Storage for FailingStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.append(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        if self.is_failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("the disk is gone"));
        }
        self.file.sync()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.truncate(len)
    }
}

/// When the disk refuses puts that were already answered, the writer
/// says so from then on, whichever write found out: the call that
/// writes them, or a commit that carried them. Whoever serves must stop
/// then, since readers see values no restart brings back; and no
/// further put is answered.
#[test]
fn a_writer_says_when_answered_puts_were_lost() {
    for found_by in ["sync", "commit"] {
        let dir = TempDir::new(&format!("lost-{found_by}"));
        drop(store(&dir));
        let is_failing = Arc::new(AtomicBool::new(false));
        let storage = FailingStorage {
            file: FileStorage::open(&dir.log()).expect("opens file"),
            is_failing: Arc::clone(&is_failing),
        };
        let store = Store::open(storage, SystemClock).expect("opens store");
        let committer = Committer::start(store);
        let writer = committer.writer();
        writer
            .put(vec![set("cache/top", "1")])
            .wait()
            .expect("puts");
        assert!(!writer.lost_answered_puts());

        is_failing.store(true, Ordering::SeqCst);
        if found_by == "sync" {
            writer.sync_puts().expect_err("the disk is gone");
        } else {
            writer
                .commit(MAIN_BRANCH, vec![set("player/1/gold", "5")], by("game"))
                .expect_err("the disk is gone");
        }

        assert!(writer.lost_answered_puts(), "{found_by}");
        writer
            .put(vec![set("cache/top", "2")])
            .wait()
            .expect_err("no put is answered once the log refused a write");
    }
}

/// A failed write that carried no answered put loses nothing answered.
#[test]
fn a_failed_write_alone_loses_no_answered_put() {
    let dir = TempDir::new("alone");
    drop(store(&dir));
    let is_failing = Arc::new(AtomicBool::new(false));
    let storage = FailingStorage {
        file: FileStorage::open(&dir.log()).expect("opens file"),
        is_failing: Arc::clone(&is_failing),
    };
    let committer = Committer::start(Store::open(storage, SystemClock).expect("opens store"));
    let writer = committer.writer();
    is_failing.store(true, Ordering::SeqCst);

    writer
        .commit(MAIN_BRANCH, vec![set("player/1/gold", "5")], by("game"))
        .expect_err("the disk is gone");

    assert!(!writer.lost_answered_puts());
}
