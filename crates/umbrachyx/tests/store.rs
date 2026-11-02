//! The store on a real file: commits, branches, the last change of each cell
//! and the history of a key, before and after closing and opening again.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-store-{}-{name}", std::process::id()));
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

fn by(who: &str, why: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
        why: why.to_owned(),
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
        .map(|v| String::from_utf8(v.to_vec()).expect("utf-8"))
}

#[test]
fn reopening_gives_back_the_same_state() {
    let dir = TempDir::new("reopen");
    let (head, time) = {
        let mut store = open(&dir.log());
        store
            .commit(
                MAIN_BRANCH,
                vec![set("player/1/gold", "100"), set("player/1/sword", "1")],
                by("agent:onboarding", "starter kit"),
            )
            .expect("commits");
        store
            .commit(
                MAIN_BRANCH,
                vec![set("player/1/gold", "80"), remove("player/1/sword")],
                by("player:1", "sold sword"),
            )
            .expect("commits");
        (store.head(MAIN_BRANCH), store.last_time())
    };

    let store = open(&dir.log());

    assert_eq!(store.head(MAIN_BRANCH), head);
    assert_eq!(store.last_time(), time);
    assert_eq!(
        value(&store, MAIN_BRANCH, "player/1/gold").as_deref(),
        Some("80")
    );
    assert_eq!(value(&store, MAIN_BRANCH, "player/1/sword"), None);
}

#[test]
fn writes_on_a_branch_are_not_seen_on_main() {
    let dir = TempDir::new("branch");
    let mut store = open(&dir.log());
    store
        .commit(
            MAIN_BRANCH,
            vec![set("event/reward", "10")],
            by("ops", "live"),
        )
        .expect("commits");
    store
        .create_branch("event/test", MAIN_BRANCH)
        .expect("branches");

    store
        .commit(
            "event/test",
            vec![set("event/reward", "500")],
            by("agent:event", "try a bigger reward"),
        )
        .expect("commits on the branch");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("event/start", "today")],
            by("ops", "schedule"),
        )
        .expect("commits on main");

    assert_eq!(
        value(&store, MAIN_BRANCH, "event/reward").as_deref(),
        Some("10")
    );
    assert_eq!(
        value(&store, "event/test", "event/reward").as_deref(),
        Some("500")
    );
    // The branch started where main was; later commits to main stay there.
    assert_eq!(value(&store, "event/test", "event/start"), None);

    let reopened = {
        drop(store);
        open(&dir.log())
    };
    assert_eq!(
        value(&reopened, MAIN_BRANCH, "event/reward").as_deref(),
        Some("10")
    );
    assert_eq!(
        value(&reopened, "event/test", "event/reward").as_deref(),
        Some("500")
    );
}

#[test]
fn each_cell_knows_the_commit_that_last_changed_it() {
    let dir = TempDir::new("cell");
    let mut store = open(&dir.log());
    let first = store
        .commit(
            MAIN_BRANCH,
            vec![set("a", "1"), set("b", "1")],
            by("agent:x", "first"),
        )
        .expect("commits");
    let second = store
        .commit(MAIN_BRANCH, vec![set("b", "2")], by("agent:y", "second"))
        .expect("commits");

    let a = store.cell(MAIN_BRANCH, b"a").expect("a has a cell");
    let b = store.cell(MAIN_BRANCH, b"b").expect("b has a cell");

    assert_eq!(a.changed_by, first);
    assert_eq!(b.changed_by, second);
    let info = store.commit_info(&second).expect("commit is known");
    assert_eq!(info.provenance.who, "agent:y");
    assert_eq!(info.parent, Some(first));
}

#[test]
fn a_key_has_a_history_including_its_removal() {
    let dir = TempDir::new("history");
    let ids = {
        let mut store = open(&dir.log());
        let granted = store
            .commit(
                MAIN_BRANCH,
                vec![set("player/7/ring", "rare")],
                by("agent:drop", "boss drop"),
            )
            .expect("commits");
        store
            .commit(
                MAIN_BRANCH,
                vec![set("player/8/ring", "rare")],
                by("agent:drop", "unrelated"),
            )
            .expect("commits");
        let upgraded = store
            .commit(
                MAIN_BRANCH,
                vec![set("player/7/ring", "epic")],
                by("agent:forge", "upgrade"),
            )
            .expect("commits");
        let removed = store
            .commit(
                MAIN_BRANCH,
                vec![remove("player/7/ring")],
                by("agent:trade", "traded away"),
            )
            .expect("commits");
        vec![removed, upgraded, granted]
    };

    let store = open(&dir.log());
    let history = store.history(MAIN_BRANCH, b"player/7/ring");

    let changed_by: Vec<_> = history.iter().map(|cell| cell.changed_by).collect();
    let values: Vec<_> = history
        .iter()
        .map(|cell| cell.value.as_ref().map(|v| v.to_vec()))
        .collect();
    assert_eq!(changed_by, ids);
    assert_eq!(
        values,
        vec![None, Some(b"epic".to_vec()), Some(b"rare".to_vec())]
    );
    let removal = store.commit_info(&ids[0]).expect("known");
    assert_eq!(removal.provenance.why, "traded away");
}

#[test]
fn bad_branch_requests_are_refused_without_stopping_the_store() {
    let dir = TempDir::new("refused");
    let mut store = open(&dir.log());
    store.create_branch("b", MAIN_BRANCH).expect("branches");

    let unknown = store.commit("nowhere", vec![set("k", "v")], by("a", "b"));
    let twice = store.create_branch("b", MAIN_BRANCH);
    let from_unknown = store.create_branch("c", "nowhere");
    let empty_name = store.create_branch("", MAIN_BRANCH);
    let after = store.commit(MAIN_BRANCH, vec![set("k", "v")], by("a", "b"));

    assert!(
        matches!(unknown, Err(StoreError::UnknownBranch(_))),
        "{unknown:?}"
    );
    assert!(
        matches!(twice, Err(StoreError::BranchExists(_))),
        "{twice:?}"
    );
    assert!(
        matches!(from_unknown, Err(StoreError::UnknownBranch(_))),
        "{from_unknown:?}"
    );
    assert!(
        matches!(empty_name, Err(StoreError::BadBranchName)),
        "{empty_name:?}"
    );
    assert!(after.is_ok(), "{after:?}");
}

#[test]
fn the_state_at_an_earlier_commit_does_not_change() {
    let dir = TempDir::new("version");
    let mut store = open(&dir.log());
    let first = store
        .commit(MAIN_BRANCH, vec![set("k", "before")], by("a", "first"))
        .expect("commits");

    store
        .commit(
            MAIN_BRANCH,
            vec![set("k", "after"), set("new", "x")],
            by("a", "second"),
        )
        .expect("commits");

    assert_eq!(store.get_at(&first, b"k").as_deref(), Some(&b"before"[..]));
    assert_eq!(store.get_at(&first, b"new"), None);
    assert_eq!(value(&store, MAIN_BRANCH, "k").as_deref(), Some("after"));
    assert_eq!(store.entries(MAIN_BRANCH).len(), 2);
}

/// Edits in one commit apply in order: the last edit of a key wins, and a
/// removal counts what earlier edits in the same commit made live.
#[test]
fn edits_of_one_key_in_one_commit_apply_in_order() {
    let dir = TempDir::new("order");
    let mut store = open(&dir.log());

    let id = store
        .commit(
            MAIN_BRANCH,
            vec![
                set("added-then-removed", "1"),
                remove("added-then-removed"),
                remove("removed-then-added"),
                set("removed-then-added", "2"),
                set("set-twice", "3"),
                set("set-twice", "4"),
                remove("never-there"),
            ],
            by("agent:x", "one commit"),
        )
        .expect("commits");

    let removed = store
        .cell(MAIN_BRANCH, b"added-then-removed")
        .expect("the removal is recorded");
    assert_eq!(removed.value, None);
    assert_eq!(removed.changed_by, id);
    assert_eq!(
        value(&store, MAIN_BRANCH, "removed-then-added").as_deref(),
        Some("2")
    );
    assert_eq!(
        value(&store, MAIN_BRANCH, "set-twice").as_deref(),
        Some("4")
    );
    assert!(store.cell(MAIN_BRANCH, b"never-there").is_none());
}
