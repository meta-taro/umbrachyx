//! A second store fed the bytes of the first one's log ends up holding the
//! same thing: the same commits under the same ids, the same branches,
//! tables, decisions and erasures, and a log of the same bytes. The data
//! is synthetic.

use std::path::{Path, PathBuf};

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{CommitRequest, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

type SealedStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-replica-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.log"))
    }

    fn keys(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.log.keys"))
    }

    fn open(&self, name: &str) -> SealedStore {
        open(&self.log(name), &self.keys(name))
    }

    /// Gives `to` the keys `from` holds now, as a replica is given them.
    fn copy_keys(&self, from: &str, to: &str) {
        std::fs::copy(self.keys(from), self.keys(to)).expect("copies the key file");
    }

    /// The log's bytes. Its store must be closed: while one is open the
    /// file is locked, and on Windows nothing else can read it.
    fn log_bytes(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.log(name)).expect("reads the log")
    }

    /// The log of a store that wrote `write` and was closed.
    fn written(&self, name: &str, write: impl FnOnce(&mut SealedStore)) -> Vec<u8> {
        let mut store = self.open(name);
        write(&mut store);
        drop(store);
        self.log_bytes(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(log: &Path, keys: &Path) -> SealedStore {
    let storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(keys));
    let keys = Keys::open(storage).expect("opens the key file");
    Store::open_sealed(
        FileStorage::open(log).expect("opens file"),
        keys,
        SystemClock,
    )
    .expect("opens")
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

/// Every kind of record: a table, branches, commits alone and written
/// together, an approval, a rejection.
fn write_history(store: &mut SealedStore) {
    store
        .declare_table(b"pos", Engine::Latest, by("ops", "positions keep no past"))
        .expect("declares");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/1/gems", "100"), set("player/2/gems", "7")],
            by("seed", "first data"),
        )
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit_all(vec![
            CommitRequest {
                branch: "agent".to_owned(),
                edits: vec![set("player/1/gems", "150")],
                provenance: by("agent-7", "ticket 8812"),
            },
            CommitRequest {
                branch: "agent".to_owned(),
                edits: vec![set("pos/1/x", "4")],
                provenance: by("agent-7", "moved"),
            },
        ])
        .expect("writes");
    let head = store.head("agent").expect("has commits");
    store
        .approve("agent", &head, by("reviewer", "checked ticket 8812"))
        .expect("approves");
    store.create_branch("other", MAIN_BRANCH).expect("branches");
    let wrong = store
        .commit("other", vec![set("player/2/gems", "0")], by("agent-9", "?"))
        .expect("commits");
    store
        .reject("other", &wrong, by("reviewer", "wrong player"))
        .expect("rejects");
}

/// What two stores holding the same thing agree on.
fn assert_same(replica: &SealedStore, primary: &SealedStore) {
    let branches: Vec<&str> = primary.branches().collect();
    assert_eq!(replica.branches().collect::<Vec<_>>(), branches);
    for branch in branches {
        assert_eq!(
            replica.head(branch),
            primary.head(branch),
            "head of {branch}"
        );
        assert_eq!(
            replica.entries(branch),
            primary.entries(branch),
            "values on {branch}"
        );
        let ids = |store: &SealedStore| -> Vec<_> {
            store.log(branch).iter().map(|info| info.id).collect()
        };
        assert_eq!(ids(replica), ids(primary), "commits of {branch}");
    }
    assert_eq!(replica.tables(), primary.tables());
    assert_eq!(replica.decisions(100), primary.decisions(100));
    assert_eq!(replica.erasures(), primary.erasures());
}

#[test]
fn a_store_fed_another_ones_log_holds_the_same() {
    let dir = TempDir::new("same");
    let frames = dir.written("primary", write_history);
    dir.copy_keys("primary", "replica");
    let primary = dir.open("primary");
    let mut replica = dir.open("replica");

    // As a connection delivers them: cut wherever, the rest with the next.
    let cut = frames.len() / 2 + 3;
    let taken = replica
        .replicate(&frames[..cut])
        .expect("takes whole records");
    assert!(taken <= cut, "only whole records are taken");
    let rest = replica.replicate(&frames[taken..]).expect("takes the rest");

    assert_eq!(taken + rest, frames.len());
    assert_same(&replica, &primary);
    drop(replica);
    assert_eq!(
        dir.log_bytes("replica"),
        frames,
        "the same log, byte for byte"
    );
    assert_same(&dir.open("replica"), &primary);
}

#[test]
fn an_erasure_reaches_the_replica_and_destroys_its_copy_of_the_key() {
    let dir = TempDir::new("erase");
    let before = dir.written("primary", write_history).len();
    dir.copy_keys("primary", "replica");
    let frames = dir.written("primary", |primary| {
        assert!(
            primary
                .erase(b"player/1", by("ops", "asked to be forgotten"))
                .expect("erases")
        );
    });
    let primary = dir.open("primary");
    let mut replica = dir.open("replica");
    replica
        .replicate(&frames[..before])
        .expect("takes the history");
    assert_eq!(
        replica.get(MAIN_BRANCH, b"player/1/gems").as_deref(),
        Some(&b"150"[..])
    );

    replica
        .replicate(&frames[before..])
        .expect("takes the erasure");

    assert_eq!(replica.get(MAIN_BRANCH, b"player/1/gems"), None);
    assert_same(&replica, &primary);
    drop(replica);
    // The key is gone from the replica's key file, not only from memory.
    let reopened = dir.open("replica");
    assert_eq!(reopened.get(MAIN_BRANCH, b"player/1/gems"), None);
    assert_eq!(
        reopened.get(MAIN_BRANCH, b"player/2/gems").as_deref(),
        Some(&b"7"[..])
    );
}

#[test]
fn records_sealed_under_keys_the_replica_lacks_are_refused_whole() {
    let dir = TempDir::new("keys");
    let frames = dir.written("primary", write_history);
    // No keys copied: nothing the primary wrote can be opened here.
    let mut replica = dir.open("replica");

    let refused = replica.replicate(&frames);

    assert!(
        matches!(refused, Err(StoreError::MissingKeys(_))),
        "{refused:?}"
    );
    assert_eq!(replica.head(MAIN_BRANCH), None, "nothing was applied");
    drop(replica);
    assert!(dir.log_bytes("replica").is_empty(), "nothing was written");
}

#[test]
fn damaged_records_are_refused() {
    let dir = TempDir::new("damage");
    let mut frames = dir.written("primary", write_history);
    dir.copy_keys("primary", "replica");
    let middle = frames.len() / 2;
    frames[middle] ^= 0x40;
    let mut replica = dir.open("replica");

    let refused = replica.replicate(&frames);

    assert!(refused.is_err(), "{refused:?}");
}

#[test]
fn a_replica_catches_up_by_asking_for_what_comes_after_the_records_it_holds() {
    let dir = TempDir::new("catch-up");
    let mut primary = dir.open("primary");
    write_history(&mut primary);
    dir.copy_keys("primary", "replica");
    let mut replica = dir.open("replica");

    // A little at a time, as over a connection; then more is written.
    let mut rounds = 0;
    while replica.log_records() < primary.log_records() {
        let frames = primary
            .log_after(replica.log_records(), 64)
            .expect("reads its log");
        let taken = replica.replicate(&frames).expect("takes them");
        assert_eq!(taken, frames.len(), "only whole records are handed out");
        rounds += 1;
    }
    assert!(rounds > 1, "the history did not fit in one piece");
    assert_same(&replica, &primary);
    // To a subject whose key the replica already has: a new subject's key
    // would have to reach it first.
    primary
        .commit(
            MAIN_BRANCH,
            vec![set("player/2/gems", "8")],
            by("seed", "more"),
        )
        .expect("commits");
    let frames = primary
        .log_after(replica.log_records(), usize::MAX)
        .expect("reads its log");
    replica.replicate(&frames).expect("takes them");

    assert_same(&replica, &primary);
    assert!(
        primary
            .log_after(replica.log_records(), usize::MAX)
            .expect("reads its log")
            .is_empty(),
        "nothing is left to send"
    );
}

#[test]
fn a_replica_and_its_primary_agree_on_the_chain_of_the_records_both_hold() {
    let dir = TempDir::new("chain");
    let mut primary = dir.open("primary");
    write_history(&mut primary);
    dir.copy_keys("primary", "replica");
    let mut replica = dir.open("replica");
    let first = primary.log_after(0, 1).expect("reads its log");
    replica.replicate(&first).expect("takes one record");
    let held = replica.log_records();
    assert!(held > 0 && held < primary.log_records());

    let replicas = replica.log_chain(held).expect("reads its log");

    assert!(replicas.is_some());
    assert_eq!(primary.log_chain(held).expect("reads its log"), replicas);
    // More than the replica holds: it has no chain to offer.
    assert_eq!(replica.log_chain(held + 1).expect("reads its log"), None);
}

#[test]
fn a_store_with_another_history_has_another_chain_after_as_many_records() {
    let dir = TempDir::new("diverged");
    let mut primary = dir.open("primary");
    write_history(&mut primary);
    let mut other = dir.open("other");
    other.create_branch("agent", MAIN_BRANCH).expect("branches");
    other
        .commit("agent", vec![set("player/1/gems", "1")], by("x", "y"))
        .expect("commits");
    let held = other.log_records();
    assert!(held <= primary.log_records());

    let theirs = other.log_chain(held).expect("reads its log");

    assert!(theirs.is_some());
    assert_ne!(primary.log_chain(held).expect("reads its log"), theirs);
}
