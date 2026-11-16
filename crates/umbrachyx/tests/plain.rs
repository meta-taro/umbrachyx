//! The plain engine: a table declared `Plain` keeps the current value of
//! each key and nothing else. A write is not a commit, it replaces the
//! value for every branch at once, and it comes back when the store is
//! opened again because it goes through the same log as everything else.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-plain-{}-{name}", std::process::id()));
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

fn remove(key: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: None,
    }
}

fn value(store: &FileStore, branch: &str, key: &str) -> Option<String> {
    store
        .get(branch, key.as_bytes())
        .map(|bytes| String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

/// A store with `session` held by the plain engine, `player` keeping its
/// history, and a branch beside `main`.
fn with_session(dir: &TempDir) -> FileStore {
    let mut store = open(&dir.log());
    store
        .declare_table(b"session", Engine::Plain, by("owner"))
        .expect("declares");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
}

#[test]
fn a_put_replaces_the_value_on_every_branch_and_makes_no_commit() {
    let dir = TempDir::new("puts");
    let mut store = with_session(&dir);
    let head = store.head(MAIN_BRANCH);

    store.put(vec![set("session/1/token", "a")]).expect("puts");
    store.put(vec![set("session/1/token", "b")]).expect("puts");

    assert_eq!(
        value(&store, MAIN_BRANCH, "session/1/token"),
        Some("b".to_owned())
    );
    assert_eq!(
        value(&store, "agent", "session/1/token"),
        Some("b".to_owned())
    );
    assert_eq!(store.head(MAIN_BRANCH), head, "a put is not a commit");
    assert_eq!(store.head("agent"), head);
}

#[test]
fn a_removed_key_is_gone_and_edits_apply_in_order() {
    let dir = TempDir::new("removed");
    let mut store = with_session(&dir);

    store
        .put(vec![
            set("session/1/token", "a"),
            set("session/2/token", "b"),
            remove("session/1/token"),
            set("session/2/token", "c"),
        ])
        .expect("puts");

    assert_eq!(store.get(MAIN_BRANCH, b"session/1/token"), None);
    assert_eq!(
        value(&store, MAIN_BRANCH, "session/2/token"),
        Some("c".to_owned())
    );
}

#[test]
fn opening_again_replays_the_puts() {
    let dir = TempDir::new("replay");
    {
        let mut store = with_session(&dir);
        for i in 0..50 {
            store
                .put(vec![set(
                    &format!("session/{}/token", i % 7),
                    &i.to_string(),
                )])
                .expect("puts");
        }
        store.put(vec![remove("session/3/token")]).expect("puts");
        store
            .commit(MAIN_BRANCH, vec![set("player/1/gold", "5")], by("game"))
            .expect("commits");
    }

    let store = open(&dir.log());

    assert_eq!(store.engine_of(b"session/1/token"), Engine::Plain);
    assert_eq!(
        value(&store, MAIN_BRANCH, "session/6/token"),
        Some("48".to_owned())
    );
    assert_eq!(
        value(&store, "agent", "session/0/token"),
        Some("49".to_owned())
    );
    assert_eq!(store.get(MAIN_BRANCH, b"session/3/token"), None);
    assert_eq!(
        value(&store, MAIN_BRANCH, "player/1/gold"),
        Some("5".to_owned())
    );
}

/// The two ways of writing do not cross: a put needs a plain table, and
/// a commit cannot touch one, since it would have no history to be in.
#[test]
fn a_put_is_refused_outside_plain_tables_and_a_commit_inside_them() {
    let dir = TempDir::new("refused");
    let mut store = with_session(&dir);

    let put = store.put(vec![set("session/1/token", "a"), set("player/1/gold", "5")]);
    let commit = store.commit(
        MAIN_BRANCH,
        vec![set("player/1/gold", "5"), set("session/1/token", "a")],
        by("game"),
    );

    assert!(matches!(put, Err(StoreError::NotPlain(key)) if key == b"player/1/gold"));
    assert!(matches!(commit, Err(StoreError::PlainTable(table)) if table == b"session"));
    assert_eq!(
        store.get(MAIN_BRANCH, b"session/1/token"),
        None,
        "nothing was written"
    );
    assert_eq!(store.get(MAIN_BRANCH, b"player/1/gold"), None);
}

/// No edits are no record: the table is still untouched afterwards, as
/// declaring it again shows, also once the log is replayed.
#[test]
fn a_put_of_nothing_writes_nothing() {
    let dir = TempDir::new("nothing");
    let mut store = with_session(&dir);

    store.put(Vec::new()).expect("puts nothing");

    assert!(
        store
            .declare_table(b"session", Engine::History, by("owner"))
            .is_ok()
    );
    drop(store);
    let mut store = open(&dir.log());
    assert!(
        store
            .declare_table(b"session", Engine::Plain, by("owner"))
            .is_ok()
    );
}

#[test]
fn a_plain_table_that_was_written_cannot_be_declared_again() {
    let dir = TempDir::new("in-use");
    let mut store = with_session(&dir);
    store.put(vec![set("session/1/token", "a")]).expect("puts");

    let refused = store.declare_table(b"session", Engine::History, by("owner"));

    assert!(matches!(refused, Err(StoreError::TableInUse(table)) if table == b"session"));
    drop(store);
    let mut store = open(&dir.log());
    let refused = store.declare_table(b"session", Engine::Latest, by("owner"));
    assert!(
        matches!(refused, Err(StoreError::TableInUse(_))),
        "also after replay"
    );
}

#[test]
fn the_keys_of_a_plain_table_are_listed_with_the_others() {
    let dir = TempDir::new("entries");
    let mut store = with_session(&dir);
    store
        .commit(MAIN_BRANCH, vec![set("player/1/gold", "5")], by("game"))
        .expect("commits");
    store
        .put(vec![
            set("session/2/token", "b"),
            set("session/1/token", "a"),
        ])
        .expect("puts");

    let keys: Vec<String> = store
        .entries("agent")
        .into_iter()
        .map(|(key, _)| String::from_utf8(key.to_vec()).expect("utf-8"))
        .collect();

    assert_eq!(
        keys,
        ["session/1/token", "session/2/token"],
        "agent branched before the commit"
    );
    let keys: Vec<String> = store
        .entries(MAIN_BRANCH)
        .into_iter()
        .map(|(key, _)| String::from_utf8(key.to_vec()).expect("utf-8"))
        .collect();
    assert_eq!(
        keys,
        ["player/1/gold", "session/1/token", "session/2/token"]
    );
}
