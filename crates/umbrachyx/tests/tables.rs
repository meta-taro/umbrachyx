//! Declaring which engine holds a table: the declaration is a record in the
//! log, so it comes back when the store is opened again, and a table that
//! was never declared keeps its history as every table did before.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-tables-{}-{name}", std::process::id()));
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

fn declared(entries: &[(&str, Engine)]) -> BTreeMap<Vec<u8>, Engine> {
    entries
        .iter()
        .map(|(name, engine)| (name.as_bytes().to_vec(), *engine))
        .collect()
}

#[test]
fn a_table_never_declared_keeps_its_history() {
    let dir = TempDir::new("undeclared");
    let mut store = open(&dir.log());
    store
        .commit(MAIN_BRANCH, vec![set("player/1/gold", "100")], by("agent"))
        .expect("commits");

    assert!(store.tables().is_empty());
    assert_eq!(store.engine_of(b"player/1/gold"), Engine::History);
}

#[test]
fn a_declaration_comes_back_when_the_store_is_opened_again() {
    let dir = TempDir::new("reopen");
    {
        let mut store = open(&dir.log());
        store
            .declare_table(b"position", Engine::Latest, by("ops:schema"))
            .expect("declares");
        store
            .commit(MAIN_BRANCH, vec![set("position/7/x", "12")], by("game"))
            .expect("commits");
    }

    let store = open(&dir.log());

    assert_eq!(store.tables(), &declared(&[("position", Engine::Latest)]));
    assert_eq!(store.engine_of(b"position/7/x"), Engine::Latest);
    assert_eq!(store.engine_of(b"player/7/gold"), Engine::History);
    assert_eq!(
        store.get(MAIN_BRANCH, b"position/7/x").as_deref(),
        Some(&b"12"[..])
    );
}

#[test]
fn a_table_can_be_declared_again_before_anything_is_written_to_it() {
    let dir = TempDir::new("again");
    {
        let mut store = open(&dir.log());
        store
            .declare_table(b"session", Engine::Latest, by("ops:schema"))
            .expect("declares");
        store
            .declare_table(b"session", Engine::History, by("ops:schema"))
            .expect("declares again");
    }

    let store = open(&dir.log());

    assert_eq!(store.tables(), &declared(&[("session", Engine::History)]));
}

/// Changing how a table is held once it has keys would need the table
/// rebuilt from the log; until that is built, it is refused, and nothing is
/// written.
#[test]
fn a_table_already_written_cannot_be_declared() {
    let dir = TempDir::new("in-use");
    {
        let mut store = open(&dir.log());
        store
            .commit(MAIN_BRANCH, vec![set("player/1/gold", "100")], by("agent"))
            .expect("commits");
        store.create_branch("event", MAIN_BRANCH).expect("branches");
        store
            .commit("event", vec![set("bonus/1/gems", "5")], by("agent"))
            .expect("commits");
        store
            .commit(MAIN_BRANCH, vec![set("item/1/sword", "1")], by("agent"))
            .expect("commits");
        store
            .commit(MAIN_BRANCH, vec![remove("item/1/sword")], by("agent"))
            .expect("commits");

        for table in ["player", "bonus", "item"] {
            let refused = store.declare_table(table.as_bytes(), Engine::Latest, by("ops"));
            assert!(
                matches!(&refused, Err(StoreError::TableInUse(name)) if name == table.as_bytes()),
                "{table}: {refused:?}"
            );
        }
        store
            .commit(MAIN_BRANCH, vec![set("player/1/gold", "90")], by("agent"))
            .expect("the store keeps working");
    }

    let store = open(&dir.log());

    assert!(store.tables().is_empty());
}

#[test]
fn a_table_name_is_one_part_of_a_key() {
    let dir = TempDir::new("names");
    let mut store = open(&dir.log());

    for name in ["", "player/42"] {
        let refused = store.declare_table(name.as_bytes(), Engine::Latest, by("ops"));
        assert!(
            matches!(refused, Err(StoreError::BadTableName)),
            "{name:?}: {refused:?}"
        );
    }
    assert!(store.tables().is_empty());
}

/// A key of one part, like `config`, is in no table, so it keeps its
/// history whatever is declared.
#[test]
fn a_key_of_one_part_is_in_no_table() {
    let dir = TempDir::new("one-part");
    let mut store = open(&dir.log());
    store
        .declare_table(b"config", Engine::Latest, by("ops:schema"))
        .expect("declares");

    assert_eq!(store.engine_of(b"config"), Engine::History);
    assert_eq!(store.engine_of(b"config/limits"), Engine::Latest);
}
