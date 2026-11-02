//! The committer: writers on many threads, one sync for the writes that
//! arrive together, and each writer answered only once its write is durable.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use umbrachyx::log::{Edit, FileStorage, LogError, MAX_PAYLOAD_LEN, Storage};
use umbrachyx::store::{
    CommitId, Committer, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock, Writer,
};

const WRITERS: usize = 8;
const COMMITS_PER_WRITER: usize = 40;
/// Long enough that writers pile up behind each sync, so groups form on
/// any machine.
const SLOW_SYNC: Duration = Duration::from_millis(5);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-committer-{}-{name}", std::process::id()));
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

/// The file, with every sync counted and slowed down.
struct CountingStorage {
    file: FileStorage,
    syncs: Arc<AtomicUsize>,
}

impl Storage for CountingStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.append(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        thread::sleep(SLOW_SYNC);
        self.syncs.fetch_add(1, Ordering::SeqCst);
        self.file.sync()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.truncate(len)
    }
}

fn open_counting(path: &Path) -> (Store<CountingStorage, SystemClock>, Arc<AtomicUsize>) {
    let syncs = Arc::new(AtomicUsize::new(0));
    let storage = CountingStorage {
        file: FileStorage::open(path).expect("opens file"),
        syncs: Arc::clone(&syncs),
    };
    let store = Store::open(storage, SystemClock).expect("opens store");
    (store, syncs)
}

fn by(who: String) -> Provenance {
    Provenance {
        who,
        why: "concurrent write".to_owned(),
        approved_by: None,
    }
}

fn set(key: &str, value: &[u8]) -> Vec<Edit> {
    vec![Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.to_vec()),
    }]
}

/// Every acknowledged write is in the log, in an order each writer agrees
/// with, and the writes that arrived together shared a sync.
#[test]
fn concurrent_writes_share_syncs_and_all_survive() {
    let dir = TempDir::new("concurrent");
    let (store, syncs) = open_counting(&dir.log());
    let opening_syncs = syncs.load(Ordering::SeqCst);
    let committer = Committer::start(store);

    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let handle = committer.writer();
            thread::spawn(move || {
                (0..COMMITS_PER_WRITER)
                    .map(|n| {
                        let key = format!("writer/{writer}/{n:03}");
                        handle
                            .commit(MAIN_BRANCH, set(&key, b"v"), by(format!("w{writer}")))
                            .expect("commits")
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let acknowledged: Vec<Vec<_>> = writers
        .into_iter()
        .map(|writer| writer.join().expect("writer finishes"))
        .collect();
    let store = committer.stop();

    let commits = WRITERS * COMMITS_PER_WRITER;
    let used = syncs.load(Ordering::SeqCst) - opening_syncs;
    assert!(used * 2 <= commits, "{used} syncs for {commits} commits");
    drop(store);
    let reopened =
        Store::open(FileStorage::open(&dir.log()).expect("opens"), SystemClock).expect("reopens");
    let log: Vec<_> = reopened
        .log(MAIN_BRANCH)
        .iter()
        .map(|info| info.id)
        .collect();
    assert_eq!(log.len(), commits);
    for ids in &acknowledged {
        // `log` runs newest first; each writer's commits appear in its order.
        let positions: Vec<usize> = ids
            .iter()
            .map(|id| {
                log.iter()
                    .position(|logged| logged == id)
                    .expect("in the log")
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] > pair[1]));
    }
    for writer in 0..WRITERS {
        for n in 0..COMMITS_PER_WRITER {
            let key = format!("writer/{writer}/{n:03}");
            assert!(reopened.get(MAIN_BRANCH, key.as_bytes()).is_some(), "{key}");
        }
    }
}

/// A write too large for the log fails alone; writes queued with it still
/// succeed.
#[test]
fn a_write_too_large_fails_alone() {
    let dir = TempDir::new("too-large");
    let (store, _) = open_counting(&dir.log());
    let committer = Committer::start(store);
    let large_edits = set("large", &vec![0; MAX_PAYLOAD_LEN]);
    // Holds the committer in a sync while the others queue up behind it,
    // so they are read as one group.
    let blocker = spawn_commit(&committer.writer(), "first".to_owned(), set("first", b"v"));
    thread::sleep(SLOW_SYNC / 5);

    let large = spawn_commit(&committer.writer(), "large".to_owned(), large_edits);
    let small: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let key = format!("small/{writer}");
            spawn_commit(&committer.writer(), key.clone(), set(&key, b"v"))
        })
        .collect();

    assert!(matches!(
        large.join().expect("finishes"),
        Err(StoreError::Log(LogError::RecordTooLarge { .. }))
    ));
    for writer in small.into_iter().chain([blocker]) {
        writer
            .join()
            .expect("finishes")
            .expect("other writes commit");
    }
    let store = committer.stop();
    assert!(store.get(MAIN_BRANCH, b"large").is_none());
}

fn spawn_commit(
    writer: &Writer,
    who: String,
    edits: Vec<Edit>,
) -> thread::JoinHandle<Result<CommitId, StoreError>> {
    let writer = writer.clone();
    thread::spawn(move || writer.commit(MAIN_BRANCH, edits, by(who)))
}

#[test]
fn errors_reach_the_writer_and_a_stopped_committer_refuses_writes() {
    let dir = TempDir::new("errors");
    let (store, _) = open_counting(&dir.log());
    let committer = Committer::start(store);
    let handle = committer.writer();

    let unknown = handle.commit("missing", set("a", b"1"), by("w".to_owned()));
    assert!(matches!(unknown, Err(StoreError::UnknownBranch(name)) if name == "missing"));

    committer.stop();
    let stopped = handle.commit(MAIN_BRANCH, set("a", b"1"), by("w".to_owned()));
    assert!(matches!(stopped, Err(StoreError::Stopped)));
}
