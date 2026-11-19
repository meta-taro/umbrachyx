//! A store that answers a write only once a replica holds it: the write
//! is on the replica when its writer hears of it, a write no replica
//! held in time is said to be only here, and the writes after it are
//! refused unwritten until a replica holds everything. The data is
//! synthetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use std::path::PathBuf;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::backup::{DirSource, DirTarget, restore};
use umbrachyx::store::follow::{MAX_SENT, follow_once};
use umbrachyx::store::{
    CommitId, Committer, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock, Writer,
};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-wait-{}-{name}", std::process::id()));
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

    /// Backs `from` up and restores the backup as the store `name`: how a
    /// replica starts.
    fn copy_of(&self, from: &mut FileStore, name: &str) -> FileStore {
        let dir = self.0.join(format!("backup-for-{name}"));
        let mut target = DirTarget::open(&dir).expect("opens the backup");
        from.backup(&mut target).expect("backs up");
        // The place is locked while it is written to, and on Windows
        // nothing reads it until that ends.
        drop(target);
        let mut log = FileStorage::open(&self.log(name)).expect("opens file");
        let mut keys = FileKeyStorage::new(&self.keys(name));
        restore(&mut DirSource::new(&dir), &mut log, Some(&mut keys)).expect("restores");
        drop(log);
        self.open(name)
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

type FileCommitter = Committer<FileStorage, SystemClock>;

/// Longer than any wait here takes; a test that waits this long fails.
const LONG: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(50);

/// A primary that waits `limit` for a replica, and a copy of it that
/// follows.
fn pair(dir: &TempDir, limit: Duration) -> (FileCommitter, FileCommitter) {
    let mut store = dir.open("primary");
    commit(&mut store, "player/1/gems", "100");
    let replica = dir.copy_of(&mut store, "replica");
    (
        Committer::start_waiting_for_a_replica(store, limit),
        Committer::start_following(replica),
    )
}

/// The replica asking its primary, as its own thread does in a server,
/// until dropped.
struct Following {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Following {
    fn start(primary: &FileCommitter, replica: &FileCommitter) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (mut feed, mut follower) = (primary.writer(), replica.writer());
        let stopped = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let progress = follow_once(&mut feed, &mut follower, MAX_SENT).expect("follows");
                if progress.is_caught_up() {
                    feed.wait_for_more_records(progress.held, Duration::from_millis(20));
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Following {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the follower ends");
        }
    }
}

fn write(writer: &Writer, value: &str) -> Result<CommitId, StoreError> {
    writer.commit(
        MAIN_BRANCH,
        vec![set("player/1/gems", value)],
        by("seed", "test data"),
    )
}

#[test]
fn a_commit_is_on_a_replica_when_its_writer_hears_of_it() {
    let dir = TempDir::new("held");
    let (primary, replica) = pair(&dir, LONG);
    let _following = Following::start(&primary, &replica);
    let writer = primary.writer();
    let reader = replica.reader();

    for round in 0..50 {
        let value = round.to_string();

        write(&writer, &value).expect("commits");

        assert_eq!(
            reader.view().get(MAIN_BRANCH, b"player/1/gems").as_deref(),
            Some(value.as_bytes()),
            "round {round}"
        );
    }
    assert_eq!(writer.replicas(), Some((writer.held_records(), false)));
}

#[test]
fn commits_sent_together_from_many_threads_are_all_on_a_replica_when_answered() {
    let dir = TempDir::new("many");
    let (primary, replica) = pair(&dir, LONG);
    let _following = Following::start(&primary, &replica);
    let follower = replica.writer();

    let writers: Vec<_> = (0..4)
        .map(|thread| {
            let (writer, follower) = (primary.writer(), follower.clone());
            thread::spawn(move || {
                for round in 0..25 {
                    write(&writer, &format!("{thread}-{round}")).expect("commits");
                    // Its own record has a number no greater than this.
                    let written = writer.held_records();
                    assert!(follower.held_records() + 100 > written);
                }
            })
        })
        .collect();

    for writer in writers {
        writer.join().expect("a writer finishes");
    }
    assert_eq!(follower.held_records(), primary.writer().held_records());
}

#[test]
fn a_commit_no_replica_held_in_time_is_said_to_be_only_here_and_the_next_is_not_written() {
    let dir = TempDir::new("alone");
    let (primary, replica) = pair(&dir, SHORT);
    let writer = primary.writer();
    let before = writer.held_records();

    let only_here = write(&writer, "150");
    let refused = write(&writer, "175");
    let decision = writer.decide(|_| ());

    assert!(
        matches!(only_here, Err(StoreError::NotReplicated)),
        "{only_here:?}"
    );
    assert!(matches!(refused, Err(StoreError::NoReplica)), "{refused:?}");
    assert!(
        matches!(decision, Err(StoreError::NoReplica)),
        "{decision:?}"
    );
    // The first is in the log, and readers see it; the second is not.
    assert_eq!(writer.held_records(), before + 1);
    assert_eq!(
        primary
            .reader()
            .view()
            .get(MAIN_BRANCH, b"player/1/gems")
            .as_deref(),
        Some(&b"150"[..])
    );
    assert_eq!(writer.replicas(), Some((0, true)));

    // A replica that comes and holds everything opens it again.
    let _following = Following::start(&primary, &replica);
    let start = std::time::Instant::now();
    while writer.replicas() != Some((before + 1, false)) {
        assert!(start.elapsed() < LONG, "{:?}", writer.replicas());
        thread::sleep(Duration::from_millis(5));
    }
    write(&writer, "200").expect("commits again");
    assert_eq!(
        replica
            .reader()
            .view()
            .get(MAIN_BRANCH, b"player/1/gems")
            .as_deref(),
        Some(&b"200"[..])
    );
}

#[test]
fn a_store_that_does_not_wait_for_replicas_answers_without_one() {
    let dir = TempDir::new("no-wait");
    let mut store = dir.open("primary");
    commit(&mut store, "player/1/gems", "100");
    let primary = Committer::start(store);

    write(&primary.writer(), "150").expect("commits");

    assert_eq!(primary.writer().replicas(), None);
}
