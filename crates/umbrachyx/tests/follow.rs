//! A replica follows its primary by asking for the records after the ones
//! it holds: it ends up with the same commits under the same ids, takes
//! no writes of its own, and is sent nothing when what it holds is not
//! the start of what its primary holds. The data is synthetic.

use std::path::PathBuf;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::backup::{DirSource, DirTarget, restore};
use umbrachyx::store::follow::{
    Ask, Feed, FollowError, Follower, MAX_SENT, Primary, Refusal, Sent, follow_once,
};
use umbrachyx::store::{
    CommitId, Committer, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock,
};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-follow-{}-{name}", std::process::id()));
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

/// A primary with a few commits about two players.
fn primary(dir: &TempDir) -> FileStore {
    let mut store = dir.open("primary");
    commit(&mut store, "player/1/gems", "100");
    commit(&mut store, "player/2/gems", "7");
    store
}

/// Asks until the replica holds what the primary does.
fn catch_up(primary: &mut impl Primary, replica: &mut impl Follower, max_bytes: usize) {
    loop {
        let progress = follow_once(primary, replica, max_bytes).expect("follows");
        if progress.is_caught_up() {
            return;
        }
    }
}

fn assert_same(replica: &FileStore, primary: &FileStore) {
    assert_eq!(replica.head(MAIN_BRANCH), primary.head(MAIN_BRANCH));
    assert_eq!(replica.entries(MAIN_BRANCH), primary.entries(MAIN_BRANCH));
    assert_eq!(replica.erasures(), primary.erasures());
    assert_eq!(replica.log_records(), primary.log_records());
}

#[test]
fn a_copy_of_the_primary_follows_it_to_the_same_commits() {
    let dir = TempDir::new("same");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    for round in 0..40 {
        commit(&mut primary, "player/1/gems", &round.to_string());
    }
    primary
        .erase(b"player/2", by("ops", "asked to be forgotten"))
        .expect("erases");

    // A few bytes at a time: one record an ask.
    catch_up(&mut primary, &mut replica, 1);

    assert_same(&replica, &primary);
    assert_eq!(replica.get(MAIN_BRANCH, b"player/2/gems"), None);
    drop(replica);
    assert_same(&dir.open("replica"), &primary);
}

#[test]
fn a_store_that_is_not_a_copy_of_the_primary_is_sent_nothing() {
    let dir = TempDir::new("another");
    let mut primary = primary(&dir);
    // Made on its own, with no records: only the log's name tells it
    // apart from a replica that has not been sent anything yet.
    let mut other = dir.open("other");

    let refused = follow_once(&mut primary, &mut other, MAX_SENT);

    assert!(
        matches!(refused, Err(FollowError::Refused(Refusal::AnotherLog))),
        "{refused:?}"
    );
    assert_eq!(other.log_records(), 0);
}

#[test]
fn a_replica_written_to_on_its_own_is_sent_nothing_more() {
    let dir = TempDir::new("diverged");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut replica, "player/1/gems", "written here");
    commit(&mut primary, "player/1/gems", "written there");
    commit(&mut primary, "player/1/gems", "and again");
    let held = replica.log_records();

    let refused = follow_once(&mut primary, &mut replica, MAX_SENT);

    assert!(
        matches!(refused, Err(FollowError::Refused(Refusal::Diverged))),
        "{refused:?}"
    );
    assert_eq!(replica.log_records(), held);
}

#[test]
fn a_primary_that_holds_fewer_records_is_not_followed() {
    let dir = TempDir::new("fewer");
    let mut primary = primary(&dir);
    // An older copy of the primary, started in its place.
    let mut older = dir.copy_of(&mut primary, "older");
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/1/gems", "150");
    catch_up(&mut primary, &mut replica, MAX_SENT);
    let held = replica.log_records();

    let refused = follow_once(&mut older, &mut replica, MAX_SENT);

    assert!(
        matches!(
            refused,
            Err(FollowError::Refused(Refusal::Fewer { records })) if records == held - 1
        ),
        "{refused:?}"
    );
    assert_eq!(replica.log_records(), held);
}

#[test]
fn an_answer_that_comes_twice_or_late_changes_nothing() {
    let dir = TempDir::new("twice");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/1/gems", "150");
    commit(&mut primary, "player/1/gems", "175");
    let ask = Ask {
        held: replica.held().expect("reads its log"),
        max_bytes: 1,
    };
    let first: Sent = primary.sent(&ask).expect("reads its log").expect("answers");
    let after_first = replica.take_in(&first).expect("takes it in");
    assert_eq!(after_first, ask.held.records + 1);

    let again = replica.take_in(&first).expect("leaves it");

    assert_eq!(again, after_first);
    catch_up(&mut primary, &mut replica, MAX_SENT);
    let late = replica.take_in(&first).expect("leaves it");
    assert_eq!(late, primary.log_records());
    assert_same(&replica, &primary);
}

#[test]
fn records_under_a_key_the_replica_was_not_given_stop_it_where_it_is() {
    let dir = TempDir::new("key");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/1/gems", "150");
    // A subject the primary first heard of after the replica was made:
    // its key is in the primary's key file only.
    commit(&mut primary, "player/3/gems", "1");
    let held = replica.log_records();

    let first = follow_once(&mut primary, &mut replica, 1).expect("takes what it can read");
    let stopped = follow_once(&mut primary, &mut replica, 1);

    assert_eq!(first.held, held + 1);
    assert!(
        matches!(stopped, Err(FollowError::Store(StoreError::MissingKeys(1)))),
        "{stopped:?}"
    );
    assert_eq!(replica.log_records(), held + 1);
    assert_eq!(
        replica.get(MAIN_BRANCH, b"player/1/gems").as_deref(),
        Some(&b"150"[..])
    );
}

#[test]
fn readers_of_a_following_store_see_what_it_takes_in_and_it_takes_no_writes() {
    let dir = TempDir::new("committer");
    let mut primary = primary(&dir);
    let replica = Committer::start_following(dir.copy_of(&mut primary, "replica"));
    let mut follower = replica.writer();
    let reader = replica.reader();
    commit(&mut primary, "player/1/gems", "150");
    assert_eq!(
        reader.view().get(MAIN_BRANCH, b"player/1/gems").as_deref(),
        Some(&b"100"[..])
    );

    catch_up(&mut primary, &mut follower, MAX_SENT);

    assert_eq!(
        reader.view().get(MAIN_BRANCH, b"player/1/gems").as_deref(),
        Some(&b"150"[..])
    );
    let writer = replica.writer();
    assert!(writer.is_following());
    let commit = writer.commit(MAIN_BRANCH, vec![set("player/1/gems", "1")], by("a", "b"));
    assert!(matches!(commit, Err(StoreError::ReadOnly)), "{commit:?}");
    let put = writer.put(vec![set("player/1/gems", "1")]).wait();
    assert!(matches!(put, Err(StoreError::ReadOnly)), "{put:?}");
    let decision = writer.decide(|_| ());
    assert!(
        matches!(decision, Err(StoreError::ReadOnly)),
        "{decision:?}"
    );
    assert_same(&replica.stop(), &primary);
}

#[test]
fn a_store_that_writes_for_itself_takes_in_no_other_stores_records() {
    let dir = TempDir::new("writes");
    let mut primary = primary(&dir);
    let copy = Committer::start(dir.copy_of(&mut primary, "copy"));
    commit(&mut primary, "player/1/gems", "150");

    let refused = follow_once(&mut primary, &mut copy.writer(), MAX_SENT);

    assert!(
        matches!(refused, Err(FollowError::Store(StoreError::NotFollowing))),
        "{refused:?}"
    );
    assert_eq!(copy.stop().log_records(), primary.log_records() - 1);
}

#[test]
fn a_primary_feeds_a_replica_while_others_write_to_it() {
    let dir = TempDir::new("serving");
    let mut store = primary(&dir);
    let replica = Committer::start_following(dir.copy_of(&mut store, "replica"));
    let primary = Committer::start(store);
    let writer = primary.writer();
    let writing = std::thread::spawn(move || {
        for round in 0..200 {
            writer
                .commit(
                    MAIN_BRANCH,
                    vec![set("player/1/gems", &round.to_string())],
                    by("seed", "test data"),
                )
                .expect("commits");
        }
    });

    // Follows while the writer writes, then to the end.
    let (mut feed, mut follower) = (primary.writer(), replica.writer());
    while !writing.is_finished() {
        follow_once(&mut feed, &mut follower, 256).expect("follows");
    }
    writing.join().expect("writer finishes");
    catch_up(&mut feed, &mut follower, MAX_SENT);

    assert_eq!(
        replica
            .reader()
            .view()
            .get(MAIN_BRANCH, b"player/1/gems")
            .as_deref(),
        Some(&b"199"[..])
    );
    assert_same(&replica.stop(), &primary.stop());
}

#[test]
fn whoever_waits_for_more_records_is_told_when_one_is_written_and_not_before() {
    use std::time::{Duration, Instant};

    let dir = TempDir::new("waits");
    let store = Committer::start(primary(&dir));
    let waiter = store.writer();
    let held = store.writer().held_records();

    let nothing = waiter.wait_for_more_records(held, Duration::from_millis(50));

    assert!(!nothing, "nothing was written");
    let writer = store.writer();
    let writing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        writer
            .commit(MAIN_BRANCH, vec![set("player/1/gems", "150")], by("a", "b"))
            .expect("commits");
    });
    let start = Instant::now();
    let more = waiter.wait_for_more_records(held, Duration::from_secs(30));
    assert!(more, "a record was written");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    // One that is there already is not waited for.
    assert!(waiter.wait_for_more_records(held, Duration::ZERO));
    writing.join().expect("writer finishes");
}
