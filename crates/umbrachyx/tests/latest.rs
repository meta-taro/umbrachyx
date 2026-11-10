//! The latest-value engine: a table declared `Latest` keeps only the current
//! value of each key and the commit that last changed it. It goes through
//! the same log as everything else, so it comes back when the store is
//! opened again, and a commit can write it and a history table at once.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Merged, Provenance, Reverted, Store, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-latest-{}-{name}", std::process::id()));
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

/// A store with `pos` held by the latest-value engine and `player` keeping
/// its history.
fn with_pos(dir: &TempDir) -> FileStore {
    let mut store = open(&dir.log());
    store
        .declare_table(b"pos", Engine::Latest, by("owner"))
        .expect("declares");
    store
}

#[test]
fn a_latest_table_reads_back_what_was_written() {
    let dir = TempDir::new("reads");
    let mut store = with_pos(&dir);

    let first = store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "10")], by("game"))
        .expect("commits");
    let second = store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "11")], by("game"))
        .expect("commits");

    assert_ne!(first, second);
    assert_eq!(value(&store, MAIN_BRANCH, "pos/1/x"), Some("11".to_owned()));
    let cell = store.cell(MAIN_BRANCH, b"pos/1/x").expect("a cell");
    assert_eq!(cell.changed_by, second);
}

#[test]
fn a_removed_key_is_gone() {
    let dir = TempDir::new("removed");
    let mut store = with_pos(&dir);
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "10")], by("game"))
        .expect("commits");

    store
        .commit(MAIN_BRANCH, vec![remove("pos/1/x")], by("game"))
        .expect("commits");

    assert_eq!(store.get(MAIN_BRANCH, b"pos/1/x"), None);
    assert_eq!(store.cell(MAIN_BRANCH, b"pos/1/x"), None);
}

#[test]
fn opening_again_replays_the_latest_table() {
    let dir = TempDir::new("replay");
    let head = {
        let mut store = with_pos(&dir);
        for i in 0..50 {
            store
                .commit(
                    MAIN_BRANCH,
                    vec![set(&format!("pos/{}/x", i % 7), &i.to_string())],
                    by("game"),
                )
                .expect("commits");
        }
        store
            .commit(MAIN_BRANCH, vec![remove("pos/3/x")], by("game"))
            .expect("commits")
    };

    let store = open(&dir.log());

    assert_eq!(store.engine_of(b"pos/1/x"), Engine::Latest);
    assert_eq!(store.head(MAIN_BRANCH), Some(head));
    assert_eq!(value(&store, MAIN_BRANCH, "pos/6/x"), Some("48".to_owned()));
    assert_eq!(value(&store, MAIN_BRANCH, "pos/0/x"), Some("49".to_owned()));
    assert_eq!(store.get(MAIN_BRANCH, b"pos/3/x"), None);
}

#[test]
fn one_commit_writes_both_engines_and_only_history_keeps_the_past() {
    let dir = TempDir::new("both");
    let mut store = with_pos(&dir);
    store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "1"), set("player/1/gold", "100")],
            by("game"),
        )
        .expect("commits");
    let head = store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "2"), set("player/1/gold", "90")],
            by("game"),
        )
        .expect("commits");

    assert_eq!(store.history(MAIN_BRANCH, b"player/1/gold").len(), 2);
    assert!(store.history(MAIN_BRANCH, b"pos/1/x").is_empty());
    let entries: Vec<(String, String)> = store
        .entries(MAIN_BRANCH)
        .into_iter()
        .map(|(k, v)| {
            (
                String::from_utf8(k.to_vec()).expect("utf-8"),
                String::from_utf8(v.to_vec()).expect("utf-8"),
            )
        })
        .collect();
    assert_eq!(
        entries,
        vec![
            ("player/1/gold".to_owned(), "90".to_owned()),
            ("pos/1/x".to_owned(), "2".to_owned()),
        ]
    );
    assert_eq!(store.entries_at(&head), store.entries(MAIN_BRANCH));
}

#[test]
fn commits_to_a_latest_table_on_main_keep_no_cells() {
    let dir = TempDir::new("no-cells");
    let mut store = with_pos(&dir);

    for i in 0..200 {
        store
            .commit(
                MAIN_BRANCH,
                vec![set(&format!("pos/{i}/x"), "1")],
                by("game"),
            )
            .expect("commits");
    }

    assert_eq!(store.cells_kept(), 0);
}

#[test]
fn a_branch_writes_apart_from_main() {
    let dir = TempDir::new("branch");
    let mut store = with_pos(&dir);
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "1")], by("game"))
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");

    store
        .commit(
            "agent",
            vec![set("pos/1/x", "5"), set("pos/2/x", "7")],
            by("agent"),
        )
        .expect("commits");

    assert_eq!(value(&store, MAIN_BRANCH, "pos/1/x"), Some("1".to_owned()));
    assert_eq!(store.get(MAIN_BRANCH, b"pos/2/x"), None);
    assert_eq!(value(&store, "agent", "pos/1/x"), Some("5".to_owned()));
    assert_eq!(value(&store, "agent", "pos/2/x"), Some("7".to_owned()));
}

#[test]
fn a_view_reads_and_lists_latest_keys() {
    let dir = TempDir::new("view");
    let mut store = with_pos(&dir);
    store
        .commit(
            MAIN_BRANCH,
            vec![
                set("pos/1/x", "1"),
                set("pos/2/x", "2"),
                set("player/1/gold", "3"),
            ],
            by("game"),
        )
        .expect("commits");

    let view = store.view();
    let (mut keys, more) = view.keys(MAIN_BRANCH, 0, 100);
    keys.sort();

    assert_eq!(
        view.get(MAIN_BRANCH, b"pos/2/x").as_deref(),
        Some(&b"2"[..])
    );
    assert!(view.contains(MAIN_BRANCH, b"pos/1/x"));
    assert!(!more);
    let keys: Vec<&[u8]> = keys.iter().map(|k| &k[..]).collect();
    assert_eq!(keys, vec![&b"player/1/gold"[..], b"pos/1/x", b"pos/2/x"]);
}

#[test]
fn a_view_pages_through_latest_keys_without_missing_any() {
    let dir = TempDir::new("pages");
    let mut store = with_pos(&dir);
    let edits = (0..250).map(|i| set(&format!("pos/{i}/x"), "v")).collect();
    store
        .commit(MAIN_BRANCH, edits, by("game"))
        .expect("commits");

    let view = store.view();
    let mut seen = Vec::new();
    let mut skip = 0;
    loop {
        let (keys, more) = view.keys(MAIN_BRANCH, skip, 40);
        skip += 40;
        seen.extend(keys);
        if !more {
            break;
        }
    }
    seen.sort();
    seen.dedup();

    assert_eq!(seen.len(), 250);
}

#[test]
fn merging_a_branch_brings_its_latest_values() {
    let dir = TempDir::new("merge");
    let mut store = with_pos(&dir);
    store
        .commit(
            MAIN_BRANCH,
            vec![
                set("pos/1/x", "1"),
                set("pos/2/x", "2"),
                set("pos/3/x", "3"),
            ],
            by("game"),
        )
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit(
            "agent",
            vec![set("pos/1/x", "10"), remove("pos/2/x")],
            by("agent"),
        )
        .expect("commits");
    store
        .commit(MAIN_BRANCH, vec![set("pos/3/x", "30")], by("game"))
        .expect("commits");

    let merged = store
        .merge(MAIN_BRANCH, "agent", by("owner"))
        .expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "pos/1/x"), Some("10".to_owned()));
    assert_eq!(store.get(MAIN_BRANCH, b"pos/2/x"), None);
    assert_eq!(value(&store, MAIN_BRANCH, "pos/3/x"), Some("30".to_owned()));
}

#[test]
fn a_key_both_sides_changed_differently_is_a_conflict() {
    let dir = TempDir::new("conflict");
    let mut store = with_pos(&dir);
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "1")], by("game"))
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit("agent", vec![set("pos/1/x", "10")], by("agent"))
        .expect("commits");
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "20")], by("game"))
        .expect("commits");
    let head = store.head(MAIN_BRANCH);

    let merged = store
        .merge(MAIN_BRANCH, "agent", by("owner"))
        .expect("merges");

    let Merged::Conflicts(conflicts) = merged else {
        panic!("expected a conflict, got {merged:?}");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].base.as_deref(), Some(&b"1"[..]));
    assert_eq!(conflicts[0].ours.as_deref(), Some(&b"20"[..]));
    assert_eq!(conflicts[0].theirs.as_deref(), Some(&b"10"[..]));
    assert_eq!(store.head(MAIN_BRANCH), head, "nothing was written");
}

/// A branch that took `main`'s value of `pos/1/x` in an earlier merge, and
/// then `main` changed it again. `pos/2/x` is the branch's own change.
fn branch_merged_main_once(store: &mut FileStore) {
    store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "0"), set("pos/2/x", "0")],
            by("game"),
        )
        .expect("commits");
    store.create_branch("a", MAIN_BRANCH).expect("branches");
    store
        .commit("a", vec![set("pos/2/x", "A")], by("agent"))
        .expect("commits");
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "M1")], by("game"))
        .expect("commits");
    let first = store.merge("a", MAIN_BRANCH, by("agent")).expect("merges");
    assert!(matches!(first, Merged::Commit(_)), "{first:?}");
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "M2")], by("game"))
        .expect("commits");
}

#[test]
fn merging_main_again_takes_what_main_changed_since_the_last_merge() {
    let dir = TempDir::new("again-into-branch");
    let mut store = with_pos(&dir);
    branch_merged_main_once(&mut store);

    let merged = store.merge("a", MAIN_BRANCH, by("agent")).expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, "a", "pos/1/x"), Some("M2".to_owned()));
    assert_eq!(value(&store, "a", "pos/2/x"), Some("A".to_owned()));
}

#[test]
fn merging_back_after_taking_main_keeps_main_newer_value() {
    let dir = TempDir::new("again-into-main");
    let mut store = with_pos(&dir);
    branch_merged_main_once(&mut store);

    let merged = store.merge(MAIN_BRANCH, "a", by("owner")).expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "pos/1/x"), Some("M2".to_owned()));
    assert_eq!(value(&store, MAIN_BRANCH, "pos/2/x"), Some("A".to_owned()));
}

#[test]
fn conflicts_come_in_key_order_across_engines() {
    let dir = TempDir::new("conflict-order");
    let mut store = with_pos(&dir);
    let start = vec![set("aaa/1", "0"), set("pos/1/x", "0"), set("zzz/1", "0")];
    store
        .commit(MAIN_BRANCH, start, by("game"))
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    let theirs = vec![set("aaa/1", "T"), set("pos/1/x", "T"), set("zzz/1", "T")];
    store.commit("agent", theirs, by("agent")).expect("commits");
    let ours = vec![set("aaa/1", "O"), set("pos/1/x", "O"), set("zzz/1", "O")];
    store
        .commit(MAIN_BRANCH, ours, by("game"))
        .expect("commits");

    let merged = store
        .merge(MAIN_BRANCH, "agent", by("owner"))
        .expect("merges");

    let Merged::Conflicts(conflicts) = merged else {
        panic!("expected conflicts, got {merged:?}");
    };
    let keys: Vec<&[u8]> = conflicts.iter().map(|c| &c.key[..]).collect();
    assert_eq!(keys, vec![&b"aaa/1"[..], b"pos/1/x", b"zzz/1"]);
}

#[test]
fn the_same_change_on_both_sides_is_no_conflict() {
    let dir = TempDir::new("same");
    let mut store = with_pos(&dir);
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "1")], by("game"))
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit("agent", vec![set("pos/1/x", "9")], by("agent"))
        .expect("commits");
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "9")], by("game"))
        .expect("commits");

    let merged = store
        .merge(MAIN_BRANCH, "agent", by("owner"))
        .expect("merges");

    assert!(!matches!(merged, Merged::Conflicts(_)), "{merged:?}");
    assert_eq!(value(&store, MAIN_BRANCH, "pos/1/x"), Some("9".to_owned()));
}

#[test]
fn merging_main_into_a_branch_keeps_the_branch_changes() {
    let dir = TempDir::new("main-into-branch");
    let mut store = with_pos(&dir);
    store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "1"), set("pos/2/x", "2")],
            by("game"),
        )
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit("agent", vec![set("pos/1/x", "10")], by("agent"))
        .expect("commits");
    store
        .commit(MAIN_BRANCH, vec![set("pos/2/x", "20")], by("game"))
        .expect("commits");

    let merged = store
        .merge("agent", MAIN_BRANCH, by("agent"))
        .expect("merges");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(value(&store, "agent", "pos/1/x"), Some("10".to_owned()));
    assert_eq!(value(&store, "agent", "pos/2/x"), Some("20".to_owned()));
}

#[test]
fn the_diff_between_branch_heads_shows_latest_changes() {
    let dir = TempDir::new("diff");
    let mut store = with_pos(&dir);
    store
        .commit(MAIN_BRANCH, vec![set("pos/1/x", "1")], by("game"))
        .expect("commits");
    store.create_branch("agent", MAIN_BRANCH).expect("branches");
    store
        .commit("agent", vec![set("pos/1/x", "5")], by("agent"))
        .expect("commits");

    let main = store.head(MAIN_BRANCH);
    let agent = store.head("agent");
    let changes = store.diff(main.as_ref(), agent.as_ref()).expect("diffs");

    assert_eq!(changes.len(), 1);
    assert_eq!(&changes[0].key[..], b"pos/1/x");
    assert_eq!(changes[0].before.as_deref(), Some(&b"1"[..]));
    assert_eq!(changes[0].after.as_deref(), Some(&b"5"[..]));
    assert!(
        store
            .omitted_tables(main.as_ref(), agent.as_ref())
            .is_empty()
    );
}

#[test]
fn a_diff_into_the_past_of_main_omits_the_latest_table() {
    let dir = TempDir::new("omitted");
    let mut store = with_pos(&dir);
    let first = store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "1"), set("player/1/gold", "1")],
            by("game"),
        )
        .expect("commits");
    let second = store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "2"), set("player/1/gold", "2")],
            by("game"),
        )
        .expect("commits");

    let changes = store.diff(Some(&first), Some(&second)).expect("diffs");

    assert_eq!(changes.len(), 1, "only the history table: {changes:?}");
    assert_eq!(&changes[0].key[..], b"player/1/gold");
    assert_eq!(
        store.omitted_tables(Some(&first), Some(&second)),
        vec![b"pos".to_vec()]
    );
}

#[test]
fn undoing_a_commit_leaves_latest_keys_as_they_are() {
    let dir = TempDir::new("revert");
    let mut store = with_pos(&dir);
    store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "1"), set("player/1/gold", "1")],
            by("game"),
        )
        .expect("commits");
    let second = store
        .commit(
            MAIN_BRANCH,
            vec![set("pos/1/x", "2"), set("player/1/gold", "2")],
            by("game"),
        )
        .expect("commits");

    let reverted = store
        .revert(MAIN_BRANCH, &second, by("owner"))
        .expect("reverts");

    assert!(matches!(reverted, Reverted::Commit(_)), "{reverted:?}");
    assert_eq!(
        value(&store, MAIN_BRANCH, "player/1/gold"),
        Some("1".to_owned())
    );
    assert_eq!(value(&store, MAIN_BRANCH, "pos/1/x"), Some("2".to_owned()));
}
