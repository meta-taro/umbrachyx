//! Diffs between commits, three-way merges that detect conflicts, and
//! undoing one commit, on a real file and after reopening.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{
    CommitId, Conflict, MAIN_BRANCH, Merged, Provenance, Reverted, Store, SystemClock, ValueChange,
};
use umbrachyx::tree::Bytes;

type FileStore = Store<FileStorage, SystemClock>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-merge-{}-{name}", std::process::id()));
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

fn by(why: &str) -> Provenance {
    Provenance {
        who: "agent:test".to_owned(),
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

fn text(bytes: &Option<Bytes>) -> Option<String> {
    bytes
        .as_ref()
        .map(|v| String::from_utf8(v.to_vec()).expect("utf-8"))
}

fn value(store: &FileStore, branch: &str, key: &str) -> Option<String> {
    text(&store.get(branch, key.as_bytes()))
}

fn key_text(key: &Bytes) -> String {
    String::from_utf8(key.to_vec()).expect("utf-8")
}

fn commit(store: &mut FileStore, branch: &str, edits: Vec<Edit>) -> CommitId {
    store.commit(branch, edits, by("test")).expect("commits")
}

#[test]
fn a_diff_lists_changed_values_and_skips_rewrites() {
    let dir = TempDir::new("diff");
    let mut store = open(&dir.log());
    let first = commit(
        &mut store,
        MAIN_BRANCH,
        vec![set("a", "1"), set("b", "1"), set("c", "1")],
    );
    let second = commit(
        &mut store,
        MAIN_BRANCH,
        vec![set("a", "2"), set("b", "1"), remove("c"), set("d", "1")],
    );

    let changes = store
        .diff(Some(&first), Some(&second))
        .expect("known commits");

    let summary: Vec<_> = changes
        .iter()
        .map(
            |ValueChange {
                 key, before, after, ..
             }| (key_text(key), text(before), text(after)),
        )
        .collect();
    assert_eq!(
        summary,
        vec![
            ("a".to_owned(), Some("1".to_owned()), Some("2".to_owned())),
            ("c".to_owned(), Some("1".to_owned()), None),
            ("d".to_owned(), None, Some("1".to_owned())),
        ]
    );
    assert_eq!(store.diff(None, Some(&first)).expect("known").len(), 3);
}

#[test]
fn changes_to_different_keys_merge() {
    let dir = TempDir::new("clean");
    let mut store = open(&dir.log());
    commit(
        &mut store,
        MAIN_BRANCH,
        vec![set("gold", "100"), set("gems", "5")],
    );
    store.create_branch("event", MAIN_BRANCH).expect("branches");
    commit(
        &mut store,
        "event",
        vec![set("gems", "50"), set("badge", "halloween")],
    );
    commit(&mut store, MAIN_BRANCH, vec![set("gold", "120")]);
    let theirs = store.head("event").expect("has commits");

    let merged = store
        .merge(MAIN_BRANCH, "event", by("ship the event"))
        .expect("merges");

    let Merged::Commit(id) = merged else {
        panic!("expected a merge commit, got {merged:?}");
    };
    assert_eq!(value(&store, MAIN_BRANCH, "gold").as_deref(), Some("120"));
    assert_eq!(value(&store, MAIN_BRANCH, "gems").as_deref(), Some("50"));
    assert_eq!(
        value(&store, MAIN_BRANCH, "badge").as_deref(),
        Some("halloween")
    );
    assert_eq!(store.commit_info(&id).expect("known").merged, Some(theirs));
    // The branch merged from is untouched.
    assert_eq!(value(&store, "event", "gold").as_deref(), Some("100"));
}

#[test]
fn different_changes_to_one_key_conflict_and_nothing_is_written() {
    let dir = TempDir::new("conflict");
    let mut store = open(&dir.log());
    commit(
        &mut store,
        MAIN_BRANCH,
        vec![set("price", "100"), set("stock", "9")],
    );
    store.create_branch("sale", MAIN_BRANCH).expect("branches");
    commit(
        &mut store,
        "sale",
        vec![set("price", "80"), remove("stock")],
    );
    commit(
        &mut store,
        MAIN_BRANCH,
        vec![set("price", "110"), set("stock", "8")],
    );
    let head = store.head(MAIN_BRANCH);

    let merged = store
        .merge(MAIN_BRANCH, "sale", by("apply sale"))
        .expect("merges");

    let Merged::Conflicts(conflicts) = merged else {
        panic!("expected conflicts, got {merged:?}");
    };
    let summary: Vec<_> = conflicts
        .iter()
        .map(
            |Conflict {
                 key,
                 base,
                 ours,
                 theirs,
             }| (key_text(key), text(base), text(ours), text(theirs)),
        )
        .collect();
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(
        summary,
        vec![
            ("price".to_owned(), some("100"), some("110"), some("80")),
            ("stock".to_owned(), some("9"), some("8"), None),
        ]
    );
    assert_eq!(store.head(MAIN_BRANCH), head);
}

#[test]
fn the_same_change_on_both_sides_is_not_a_conflict() {
    let dir = TempDir::new("same");
    let mut store = open(&dir.log());
    commit(&mut store, MAIN_BRANCH, vec![set("k", "1")]);
    store.create_branch("b", MAIN_BRANCH).expect("branches");
    commit(&mut store, "b", vec![set("k", "2")]);
    commit(&mut store, MAIN_BRANCH, vec![set("k", "2")]);

    let merged = store.merge(MAIN_BRANCH, "b", by("merge")).expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "k").as_deref(), Some("2"));
}

#[test]
fn merging_again_starts_from_the_last_merge() {
    let dir = TempDir::new("again");
    let mut store = open(&dir.log());
    commit(&mut store, MAIN_BRANCH, vec![set("k", "1")]);
    store.create_branch("b", MAIN_BRANCH).expect("branches");
    commit(&mut store, "b", vec![set("k", "2")]);
    store
        .merge(MAIN_BRANCH, "b", by("first merge"))
        .expect("merges");
    commit(&mut store, MAIN_BRANCH, vec![set("k", "3")]);
    commit(&mut store, "b", vec![set("other", "x")]);

    // The base is now the branch's commit merged before, so main's later
    // change to `k` does not conflict with the branch's older one.
    let merged = store
        .merge(MAIN_BRANCH, "b", by("second merge"))
        .expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "k").as_deref(), Some("3"));
    assert_eq!(value(&store, MAIN_BRANCH, "other").as_deref(), Some("x"));
    let nothing_new = store
        .merge(MAIN_BRANCH, "b", by("third merge"))
        .expect("merges");
    assert_eq!(nothing_new, Merged::UpToDate);
}

#[test]
fn undoing_one_commit_restores_only_what_it_changed() {
    let dir = TempDir::new("revert");
    let mut store = open(&dir.log());
    commit(
        &mut store,
        MAIN_BRANCH,
        vec![set("p1/gems", "5"), set("p2/gems", "7")],
    );
    let mistake = commit(
        &mut store,
        MAIN_BRANCH,
        vec![
            set("p1/gems", "5005"),
            set("p2/gems", "5007"),
            set("p3/gems", "5000"),
        ],
    );
    commit(&mut store, MAIN_BRANCH, vec![set("p1/gold", "10")]);

    let reverted = store
        .revert(MAIN_BRANCH, &mistake, by("wrong reward amount"))
        .expect("reverts");

    assert!(matches!(reverted, Reverted::Commit(_)), "{reverted:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "p1/gems").as_deref(), Some("5"));
    assert_eq!(value(&store, MAIN_BRANCH, "p2/gems").as_deref(), Some("7"));
    assert_eq!(value(&store, MAIN_BRANCH, "p3/gems"), None);
    assert_eq!(value(&store, MAIN_BRANCH, "p1/gold").as_deref(), Some("10"));
}

#[test]
fn undoing_a_commit_whose_keys_changed_since_is_a_conflict() {
    let dir = TempDir::new("revert-conflict");
    let mut store = open(&dir.log());
    commit(&mut store, MAIN_BRANCH, vec![set("gems", "5")]);
    let reward = commit(&mut store, MAIN_BRANCH, vec![set("gems", "105")]);
    commit(&mut store, MAIN_BRANCH, vec![set("gems", "95")]);
    let head = store.head(MAIN_BRANCH);

    let reverted = store
        .revert(MAIN_BRANCH, &reward, by("undo"))
        .expect("runs");

    let Reverted::Conflicts(conflicts) = reverted else {
        panic!("expected a conflict, got {reverted:?}");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(text(&conflicts[0].ours).as_deref(), Some("95"));
    assert_eq!(text(&conflicts[0].theirs).as_deref(), Some("5"));
    assert_eq!(store.head(MAIN_BRANCH), head);
}

#[test]
fn merges_and_reverts_come_back_after_reopening() {
    let dir = TempDir::new("reopen");
    let (main_head, entries) = {
        let mut store = open(&dir.log());
        commit(&mut store, MAIN_BRANCH, vec![set("a", "1")]);
        store.create_branch("b", MAIN_BRANCH).expect("branches");
        commit(&mut store, "b", vec![set("b", "1")]);
        let mistake = commit(&mut store, MAIN_BRANCH, vec![set("a", "999")]);
        store.merge(MAIN_BRANCH, "b", by("merge")).expect("merges");
        store
            .revert(MAIN_BRANCH, &mistake, by("undo"))
            .expect("reverts");
        (store.head(MAIN_BRANCH), store.entries(MAIN_BRANCH))
    };

    let store = open(&dir.log());

    assert_eq!(store.head(MAIN_BRANCH), main_head);
    assert_eq!(store.entries(MAIN_BRANCH), entries);
    assert_eq!(value(&store, MAIN_BRANCH, "a").as_deref(), Some("1"));
    assert_eq!(value(&store, MAIN_BRANCH, "b").as_deref(), Some("1"));
}

/// The branch took main's changes in before main merges it back: the base
/// must be found through the branch's merge commit, or main's own change
/// since then would look like a conflict with the copy the branch took.
#[test]
fn the_base_is_found_through_merges_on_the_other_side() {
    let dir = TempDir::new("criss");
    let mut store = open(&dir.log());
    commit(&mut store, MAIN_BRANCH, vec![set("k", "1")]);
    store.create_branch("b", MAIN_BRANCH).expect("branches");
    commit(&mut store, MAIN_BRANCH, vec![set("x", "1")]);
    let taken = store
        .merge("b", MAIN_BRANCH, by("catch up with main"))
        .expect("merges");
    assert!(matches!(taken, Merged::Commit(_)), "{taken:?}");
    commit(&mut store, MAIN_BRANCH, vec![set("x", "2")]);
    commit(&mut store, "b", vec![set("y", "1")]);

    let merged = store
        .merge(MAIN_BRANCH, "b", by("merge back"))
        .expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "x").as_deref(), Some("2"));
    assert_eq!(value(&store, MAIN_BRANCH, "y").as_deref(), Some("1"));
}
