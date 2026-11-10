//! The store on a real file: commits, branches, the last change of each cell
//! and the history of a key, before and after closing and opening again.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{CommitRequest, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

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

    assert_eq!(
        store.get_at(&first, b"k").expect("reads").as_deref(),
        Some(&b"before"[..])
    );
    assert_eq!(store.get_at(&first, b"new").expect("reads"), None);
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

fn request(branch: &str, edits: Vec<Edit>, who: &str) -> CommitRequest {
    CommitRequest {
        branch: branch.to_owned(),
        edits,
        provenance: by(who, "grouped"),
    }
}

/// Commits written together are acknowledged together and apply in order,
/// each a commit of its own; one to an unknown branch is refused alone.
#[test]
fn commits_written_together_apply_in_order() {
    let dir = TempDir::new("group");
    let mut store = open(&dir.log());
    store.create_branch("event", MAIN_BRANCH).expect("branches");

    let results = store
        .commit_all(vec![
            request(MAIN_BRANCH, vec![set("a", "1")], "agent:1"),
            request("missing", vec![set("a", "x")], "agent:2"),
            request("event", vec![set("a", "e")], "agent:3"),
            request(MAIN_BRANCH, vec![set("a", "2"), set("b", "1")], "agent:4"),
        ])
        .expect("the group is written");

    assert_eq!(results.len(), 4);
    assert!(matches!(&results[1], Err(StoreError::UnknownBranch(name)) if name == "missing"));
    let first = *results[0].as_ref().expect("first commits");
    let last = *results[3].as_ref().expect("last commits");
    assert_eq!(store.head(MAIN_BRANCH), Some(last));
    assert_eq!(store.commit_info(&last).expect("known").parent, Some(first));
    assert_eq!(
        store.get_at(&first, b"a").expect("reads").as_deref(),
        Some(&b"1"[..])
    );
    assert_eq!(value(&store, MAIN_BRANCH, "a").as_deref(), Some("2"));
    assert_eq!(value(&store, "event", "a").as_deref(), Some("e"));
    let history: Vec<_> = store
        .history(MAIN_BRANCH, b"a")
        .iter()
        .map(|entry| entry.changed_by)
        .collect();
    assert_eq!(history, vec![last, first]);

    drop(store);
    let reopened = open(&dir.log());
    assert_eq!(reopened.head(MAIN_BRANCH), Some(last));
    assert_eq!(reopened.head("event"), results[2].as_ref().ok().copied());
    assert_eq!(value(&reopened, MAIN_BRANCH, "b").as_deref(), Some("1"));
}

#[test]
fn an_empty_group_writes_nothing() {
    let dir = TempDir::new("empty-group");
    let mut store = open(&dir.log());

    let results = store
        .commit_all(vec![request("missing", vec![set("a", "1")], "agent:1")])
        .expect("nothing to write");

    assert!(matches!(&results[..], [Err(StoreError::UnknownBranch(_))]));
    assert_eq!(std::fs::metadata(dir.log()).expect("log exists").len(), 0);
    assert!(store.commit_all(Vec::new()).expect("nothing").is_empty());
}

/// Cells come in key order, from after a key and at most as many as asked,
/// without the keys that were removed.
#[test]
fn cells_come_in_pages_without_removed_keys() {
    let dir = TempDir::new("cells");
    let mut store = open(&dir.log());
    store
        .commit(
            MAIN_BRANCH,
            vec![set("a", "1"), set("b", "2"), set("c", "3"), set("d", "4")],
            by("seed", "four keys"),
        )
        .expect("commits");
    store
        .commit(MAIN_BRANCH, vec![remove("b")], by("ops", "drop b"))
        .expect("commits");

    let keys = |after: Option<&[u8]>, limit| -> Vec<String> {
        store
            .cells(MAIN_BRANCH, after, limit)
            .into_iter()
            .map(|(key, _)| String::from_utf8(key.to_vec()).expect("utf-8"))
            .collect()
    };

    assert_eq!(keys(None, 10), ["a", "c", "d"]);
    assert_eq!(keys(None, 2), ["a", "c"]);
    assert_eq!(keys(Some(b"a"), 10), ["c", "d"]);
    assert_eq!(keys(Some(b"b"), 1), ["c"]);
    assert!(keys(Some(b"d"), 10).is_empty());
    assert!(store.cells("missing", None, 10).is_empty());
}

#[test]
fn reading_at_a_commit_the_store_never_made_is_refused() {
    let dir = TempDir::new("unknown-commit");
    let mut store = open(&dir.log());
    store
        .commit(MAIN_BRANCH, vec![set("k", "v")], by("a", "first"))
        .expect("commits");
    let unknown = [7u8; 32];

    let read = store.get_at(&unknown, b"k");

    assert!(
        matches!(read, Err(StoreError::UnknownCommit(id)) if id == unknown),
        "got {read:?}"
    );
}
