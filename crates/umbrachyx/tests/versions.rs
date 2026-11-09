//! A long history on a real file: every past state stays readable exactly
//! as it was, through diffs, key histories and a merge, while only some
//! commits keep a whole tree in memory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{CommitId, MAIN_BRANCH, Merged, Provenance, Store, SystemClock};
use umbrachyx::tree::Bytes;

type FileStore = Store<FileStorage, SystemClock>;
type Snapshot = BTreeMap<String, String>;

const SIDE: &str = "side";
/// Long enough that main passes two kept trees and the branch one.
const MAIN_COMMITS: usize = 2_200;
const SIDE_FROM: usize = 100;
/// A branch taken from `SIDE` after `SIDE` has passed a kept tree of its own.
const LATE: &str = "late";
const LATE_FROM: usize = 2_150;
const KEYS_PER_BRANCH: u64 = 16;
/// The store keeps a whole tree for one commit in this many along each
/// line of history.
const TREE_EVERY: usize = 1024;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-versions-{}-{name}", std::process::id()));
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
        why: "a long history".to_owned(),
        approved_by: None,
    }
}

/// A small deterministic generator, so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// What the test wrote, to check the store against.
#[derive(Default)]
struct Model {
    /// Every commit and the entries it left, in the order written.
    commits: Vec<(CommitId, Snapshot)>,
    /// Each commit's first parent, or `None` for the first one.
    parents: BTreeMap<CommitId, Option<CommitId>>,
    /// The keys each commit wrote, with the value or `None` for a removal.
    written: BTreeMap<CommitId, BTreeMap<String, Option<String>>>,
}

impl Model {
    fn snapshot(&self, id: &CommitId) -> &Snapshot {
        &self
            .commits
            .iter()
            .find(|(c, _)| c == id)
            .expect("known commit")
            .1
    }

    /// The changes to `key` that `head` can see along first parents,
    /// newest first: the commit and the value it left.
    fn history(&self, head: CommitId, key: &str) -> Vec<(CommitId, Option<String>)> {
        let mut changes = Vec::new();
        let mut next = Some(head);
        while let Some(id) = next {
            if let Some(value) = self.written[&id].get(key) {
                changes.push((id, value.clone()));
            }
            next = self.parents[&id];
        }
        changes
    }
}

fn entries(store: &FileStore, branch: &str) -> Snapshot {
    to_snapshot(store.entries(branch))
}

fn to_snapshot(entries: Vec<(Bytes, Bytes)>) -> Snapshot {
    entries
        .into_iter()
        .map(|(key, value)| (text(&key), text(&value)))
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("test values are text")
}

/// One to three edits to the keys of `prefix`; values never repeat, so
/// every set is a change.
fn commit(store: &mut FileStore, model: &mut Model, rng: &mut Rng, branch: &str, prefix: &str) {
    let live = entries(store, branch);
    let mut edits = Vec::new();
    let mut written = BTreeMap::new();
    for _ in 0..=rng.next() % 3 {
        let key = format!("{prefix}/{}", rng.next() % KEYS_PER_BRANCH);
        let is_live = written
            .get(&key)
            .map_or(live.contains_key(&key), Option::is_some);
        if is_live && rng.next().is_multiple_of(4) {
            edits.push(Edit {
                key: key.clone().into_bytes(),
                value: None,
            });
            written.insert(key, None);
        } else {
            let value = format!("{branch}-{}-{}", model.commits.len(), edits.len());
            edits.push(Edit {
                key: key.clone().into_bytes(),
                value: Some(value.clone().into_bytes()),
            });
            written.insert(key, Some(value));
        }
    }
    let parent = store.head(branch);
    let id = store.commit(branch, edits, by(branch)).expect("commits");
    model.parents.insert(id, parent);
    model.written.insert(id, written);
    model.commits.push((id, entries(store, branch)));
}

/// Writes the history: main, a branch taken from it partway, both going
/// on, and the branch merged back.
fn write_history(store: &mut FileStore) -> Model {
    let mut model = Model::default();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for i in 0..MAIN_COMMITS {
        if i == SIDE_FROM {
            store.create_branch(SIDE, MAIN_BRANCH).expect("branches");
        }
        commit(store, &mut model, &mut rng, MAIN_BRANCH, "m");
        if i >= SIDE_FROM && i % 2 == 0 {
            commit(store, &mut model, &mut rng, SIDE, "s");
        }
        if i == LATE_FROM {
            store.create_branch(LATE, SIDE).expect("branches");
        }
        if i >= LATE_FROM {
            commit(store, &mut model, &mut rng, LATE, "l");
        }
    }
    // The branch only wrote its own keys, so the merge has no conflict.
    let parent = store.head(MAIN_BRANCH);
    let Merged::Commit(id) = store.merge(MAIN_BRANCH, SIDE, by("merge")).expect("merges") else {
        panic!("the merge writes a commit");
    };
    let before = parent
        .map(|p| model.snapshot(&p).clone())
        .unwrap_or_default();
    let after = entries(store, MAIN_BRANCH);
    let written = after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(value))
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .chain(
            before
                .keys()
                .filter(|key| !after.contains_key(*key))
                .map(|key| (key.clone(), None)),
        )
        .collect();
    model.parents.insert(id, parent);
    model.written.insert(id, written);
    model.commits.push((id, after));
    model
}

fn assert_every_state_reads_back(store: &FileStore, model: &Model) {
    for (n, (id, snapshot)) in model.commits.iter().enumerate() {
        assert_eq!(
            &to_snapshot(store.entries_at(id)),
            snapshot,
            "entries at a commit"
        );
        // Every value of every commit would take long; one commit in five
        // still reaches each distance from a kept tree.
        if n % 5 != 0 {
            continue;
        }
        for (key, value) in snapshot {
            let got = store.get_at(id, key.as_bytes()).map(|v| text(&v));
            assert_eq!(got.as_ref(), Some(value), "a value at a commit");
        }
    }
}

type Change = (String, Option<String>, Option<String>);

fn expected_diff(before: &Snapshot, after: &Snapshot) -> Vec<Change> {
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| {
            (
                key.clone(),
                before.get(key).cloned(),
                after.get(key).cloned(),
            )
        })
        .collect()
}

fn diff(store: &FileStore, from: &CommitId, to: &CommitId) -> Vec<Change> {
    store
        .diff(Some(from), Some(to))
        .expect("diffs")
        .into_iter()
        .map(|c| {
            let value = |v: Option<Bytes>| v.map(|v| text(&v));
            (text(&c.key), value(c.before), value(c.after))
        })
        .collect()
}

fn assert_diffs_match(store: &FileStore, model: &Model) {
    let sampled: Vec<&(CommitId, Snapshot)> = model.commits.iter().step_by(37).collect();
    let first_to_last = [sampled[0], model.commits.last().expect("has commits")];
    for pair in sampled.windows(2).chain([first_to_last.as_slice()]) {
        let ((from, before), (to, after)) = (pair[0], pair[1]);
        assert_eq!(
            diff(store, from, to),
            expected_diff(before, after),
            "the diff between two commits"
        );
    }
    // What a viewer shows most: a commit against its parent.
    for (id, after) in model.commits.iter().step_by(3) {
        let Some(parent) = model.parents[id] else {
            continue;
        };
        assert_eq!(
            diff(store, &parent, id),
            expected_diff(model.snapshot(&parent), after),
            "the diff of a commit with its parent"
        );
    }
}

fn assert_histories_match(store: &FileStore, model: &Model) {
    for branch in [MAIN_BRANCH, SIDE, LATE] {
        let head = store.head(branch).expect("has commits");
        for prefix in ["m", "s", "l"] {
            for n in 0..KEYS_PER_BRANCH {
                let key = format!("{prefix}/{n}");
                let got: Vec<(CommitId, Option<String>)> = store
                    .history(branch, key.as_bytes())
                    .into_iter()
                    .map(|cell| (cell.changed_by, cell.value.map(|v| text(&v))))
                    .collect();
                assert_eq!(
                    got,
                    model.history(head, &key),
                    "history of {key} on {branch}"
                );
            }
        }
    }
}

fn assert_all(store: &FileStore, model: &Model) {
    assert_every_state_reads_back(store, model);
    assert_diffs_match(store, model);
    assert_histories_match(store, model);
}

#[test]
fn every_past_state_reads_back_exactly_after_a_long_history() {
    let dir = TempDir::new("states");
    let mut store = open(&dir.log());
    let model = write_history(&mut store);

    assert_all(&store, &model);

    drop(store);
    let store = open(&dir.log());
    assert_all(&store, &model);
}

#[test]
fn only_some_commits_keep_a_whole_tree() {
    let dir = TempDir::new("trees");
    let mut store = open(&dir.log());
    let model = write_history(&mut store);
    let commits = model.commits.len();
    // One tree in about every TREE_EVERY commits along each line of
    // history, and one more where the branch starts. Main is long enough
    // to keep two, so the history test above reads past kept trees.
    let allowed = commits / TREE_EVERY + 2;
    let at_least = MAIN_COMMITS / TREE_EVERY;

    let kept = store.trees_kept();
    assert!(
        (at_least..=allowed).contains(&kept),
        "{kept} of {commits} commits keep a whole tree; {at_least} to {allowed} should"
    );

    drop(store);
    let store = open(&dir.log());
    assert_eq!(store.trees_kept(), kept, "the same after opening again");
}
