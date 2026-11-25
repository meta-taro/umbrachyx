//! A replica that holds current values only: it follows its primary as
//! any replica does and reads every current value, on every branch, but
//! keeps no tree and no record of commits, so what it holds does not grow
//! with every write. Its log is still whole: opened as any store, the
//! same files hold the history. The data is synthetic.

use std::path::PathBuf;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::backup::{DirSource, DirTarget, restore};
use umbrachyx::store::follow::{MAX_SENT, WithKeys, follow_once};
use umbrachyx::store::{CommitId, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-current-values-{}-{name}",
            std::process::id()
        ));
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

    /// Opens the store `name` to hold current values only.
    fn open_current(&self, name: &str) -> FileStore {
        let storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(&self.keys(name)));
        let keys = Keys::open(storage).expect("opens the key file");
        Store::open_current_values(
            FileStorage::open(&self.log(name)).expect("opens file"),
            Some(keys),
            SystemClock,
        )
        .expect("opens")
    }

    /// Backs `from` up and restores the backup as the store `name`,
    /// without opening it: how a replica starts.
    fn restore_as(&self, from: &mut FileStore, name: &str) {
        let dir = self.0.join(format!("backup-for-{name}"));
        let mut target = DirTarget::open(&dir).expect("opens the backup");
        from.backup(&mut target).expect("backs up");
        drop(target);
        let mut log = FileStorage::open(&self.log(name)).expect("opens file");
        let mut keys = FileKeyStorage::new(&self.keys(name));
        restore(&mut DirSource::new(&dir), &mut log, Some(&mut keys)).expect("restores");
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

/// A primary with tables of every kind, a key in no table, a branch that
/// has gone its own way, a merge, and an erased subject.
fn primary(dir: &TempDir) -> FileStore {
    let mut store = dir.open("primary");
    store
        .declare_table(
            b"position",
            Engine::Latest,
            by("ops", "only where it is now"),
        )
        .expect("declares");
    store
        .declare_table(b"session", Engine::Plain, by("ops", "no history"))
        .expect("declares");
    commit(&mut store, "player/1/gems", "100");
    commit(&mut store, "player/2/gems", "7");
    commit(&mut store, "config", "v1");
    commit(&mut store, "position/1/x", "10");
    store
        .put(vec![set("session/1/token", "abc")])
        .expect("puts");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit(
            "agent",
            vec![set("player/1/gems", "120")],
            by("agent", "a gift"),
        )
        .expect("commits");
    commit(&mut store, "player/1/gems", "150");
    commit(&mut store, "position/1/x", "11");
    store
}

/// More of the same, after the replica was made.
fn write_more(store: &mut FileStore) {
    commit(store, "player/3/gems", "1");
    commit(store, "config", "v2");
    store
        .commit(
            "agent",
            vec![set("player/2/gems", "8")],
            by("agent", "a gift"),
        )
        .expect("commits");
    store
        .commit(
            MAIN_BRANCH,
            vec![Edit {
                key: b"player/1/gems".to_vec(),
                value: None,
            }],
            by("ops", "gone"),
        )
        .expect("commits");
    store
        .erase(b"player/2", by("ops", "asked to be forgotten"))
        .expect("erases");
    commit(store, "player/4/gems", "44");
    store
        .put(vec![set("session/1/token", "def")])
        .expect("puts");
}

fn catch_up(primary: &mut FileStore, replica: &mut FileStore) {
    loop {
        let progress = follow_once(&mut WithKeys(primary), replica, MAX_SENT).expect("follows");
        if progress.is_caught_up() {
            return;
        }
    }
}

const KEYS: [&str; 8] = [
    "player/1/gems",
    "player/2/gems",
    "player/3/gems",
    "player/4/gems",
    "config",
    "position/1/x",
    "session/1/token",
    "player/9/gems",
];

fn assert_reads_the_same(replica: &FileStore, primary: &FileStore) {
    for branch in [MAIN_BRANCH, "agent"] {
        for key in KEYS {
            assert_eq!(
                replica.get(branch, key.as_bytes()),
                primary.get(branch, key.as_bytes()),
                "{key} on {branch}"
            );
        }
        let mut held = replica.entries(branch);
        let mut wanted = primary.entries(branch);
        held.sort();
        wanted.sort();
        assert_eq!(held, wanted, "everything on {branch}");
    }
}

#[test]
fn it_reads_every_current_value_its_primary_reads_on_every_branch() {
    let dir = TempDir::new("reads");
    let mut primary = primary(&dir);
    dir.restore_as(&mut primary, "replica");
    let mut replica = dir.open_current("replica");
    assert_reads_the_same(&replica, &primary);

    write_more(&mut primary);
    catch_up(&mut primary, &mut replica);

    assert_reads_the_same(&replica, &primary);
    assert_eq!(replica.head(MAIN_BRANCH), primary.head(MAIN_BRANCH));
    assert_eq!(replica.head("agent"), primary.head("agent"));
    assert_eq!(replica.erasures(), primary.erasures());
    assert_eq!(replica.get(MAIN_BRANCH, b"player/2/gems"), None);
    // Where the last erasure is in the log, for one that reads it as
    // records arrive and for one that reads it when it opens.
    assert!(primary.erased_through().is_some());
    assert_eq!(replica.erased_through(), primary.erased_through());
    drop(replica);
    assert_eq!(
        dir.open_current("replica").erased_through(),
        primary.erased_through()
    );
}

#[test]
fn it_says_that_it_keeps_no_history_and_a_store_opened_as_usual_says_it_does() {
    let dir = TempDir::new("says");
    let mut primary = primary(&dir);
    dir.restore_as(&mut primary, "replica");

    let replica = dir.open_current("replica");

    assert!(!replica.keeps_history());
    assert!(primary.keeps_history());
}

#[test]
fn it_takes_no_writes_of_its_own() {
    let dir = TempDir::new("writes");
    let mut primary = primary(&dir);
    dir.restore_as(&mut primary, "replica");
    let mut replica = dir.open_current("replica");
    let held = replica.log_records();
    let who = || by("someone", "tries");

    let commit = replica.commit(MAIN_BRANCH, vec![set("player/1/gems", "1")], who());
    let put = replica.put(vec![set("session/1/token", "x")]);
    let branch = replica.create_branch("other", MAIN_BRANCH);
    let erase = replica.erase(b"player/1", who());
    let table = replica.declare_table(b"new", Engine::Latest, who());

    assert!(matches!(commit, Err(StoreError::ReadOnly)), "{commit:?}");
    assert!(matches!(put, Err(StoreError::ReadOnly)), "{put:?}");
    assert!(matches!(branch, Err(StoreError::ReadOnly)), "{branch:?}");
    assert!(matches!(erase, Err(StoreError::ReadOnly)), "{erase:?}");
    assert!(matches!(table, Err(StoreError::ReadOnly)), "{table:?}");
    assert_eq!(replica.log_records(), held);
    assert_reads_the_same(&replica, &primary);
}

#[test]
fn the_same_files_opened_as_usual_hold_the_whole_history() {
    let dir = TempDir::new("whole");
    let mut primary = primary(&dir);
    dir.restore_as(&mut primary, "replica");
    let mut replica = dir.open_current("replica");
    write_more(&mut primary);
    catch_up(&mut primary, &mut replica);
    drop(replica);

    let whole = dir.open("replica");

    assert!(whole.keeps_history());
    assert_reads_the_same(&whole, &primary);
    assert_eq!(whole.log_records(), primary.log_records());
    assert_eq!(whole.head(MAIN_BRANCH), primary.head(MAIN_BRANCH));
    assert_eq!(
        whole.history(MAIN_BRANCH, b"player/1/gems"),
        primary.history(MAIN_BRANCH, b"player/1/gems")
    );
    assert_eq!(whole.decisions(usize::MAX), primary.decisions(usize::MAX));
}

#[test]
fn opened_again_to_hold_current_values_it_reads_the_same() {
    let dir = TempDir::new("again");
    let mut primary = primary(&dir);
    dir.restore_as(&mut primary, "replica");
    let mut replica = dir.open_current("replica");
    write_more(&mut primary);
    catch_up(&mut primary, &mut replica);
    drop(replica);

    let again = dir.open_current("replica");

    assert_reads_the_same(&again, &primary);
    assert_eq!(again.head(MAIN_BRANCH), primary.head(MAIN_BRANCH));
}

#[test]
fn asked_about_the_past_it_refuses_where_it_can() {
    let dir = TempDir::new("past");
    let mut primary = primary(&dir);
    dir.restore_as(&mut primary, "replica");
    let replica = dir.open_current("replica");
    let head = replica.head(MAIN_BRANCH).expect("has a head");

    let named = replica.resolve(MAIN_BRANCH);
    let earlier = replica.resolve("main~1");
    let diff = replica.diff(None, Some(&head));
    let pending = replica.pending();
    let at = replica.get_at(&head, b"player/1/gems");

    assert!(matches!(named, Err(StoreError::NoHistory)), "{named:?}");
    assert!(matches!(earlier, Err(StoreError::NoHistory)), "{earlier:?}");
    assert!(matches!(diff, Err(StoreError::NoHistory)), "{diff:?}");
    assert!(matches!(pending, Err(StoreError::NoHistory)), "{pending:?}");
    assert!(matches!(at, Err(StoreError::NoHistory)), "{at:?}");
    // And the primary answers each of them.
    assert!(primary.resolve("main~1").is_ok());
    assert!(!primary.diff(None, Some(&head)).expect("diffs").is_empty());
}
