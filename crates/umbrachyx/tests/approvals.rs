//! Changes waiting on branches, and a person approving or rejecting them:
//! only the change as it was seen, recorded with who and why, and the same
//! after reopening.

use std::path::{Path, PathBuf};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{Approved, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};
use umbrachyx::tree::Bytes;

type FileStore = Store<FileStorage, SystemClock>;

const AGENT_BRANCH: &str = "agent/support";

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-approvals-{}-{name}", std::process::id()));
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

/// `main` with two keys, and an agent's branch that changed one key and
/// added another, in two commits.
fn with_a_change(path: &Path) -> FileStore {
    let mut store = open(path);
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/7/gems", "30"), set("pos/7/x", "1")],
            by("ops:seed", "first data"),
        )
        .expect("commits");
    store
        .create_branch(AGENT_BRANCH, MAIN_BRANCH)
        .expect("branches");
    store
        .commit(
            AGENT_BRANCH,
            vec![set("player/7/gems", "530")],
            by("agent:support", "ticket 8812"),
        )
        .expect("commits");
    store
        .commit(
            AGENT_BRANCH,
            vec![set("player/7/item", "sword")],
            by("agent:support", "ticket 8812, the item too"),
        )
        .expect("commits");
    store
}

#[test]
fn a_branch_with_commits_main_lacks_is_a_change_waiting() {
    let dir = TempDir::new("pending");
    let store = with_a_change(&dir.log());

    let pending = store.pending().expect("lists");

    assert_eq!(pending.len(), 1);
    let change = &pending[0];
    assert_eq!(change.branch, AGENT_BRANCH);
    assert_eq!(Some(change.head), store.head(AGENT_BRANCH));
    assert_eq!(change.authors, vec!["agent:support".to_owned()]);
    assert_eq!(change.why, "ticket 8812, the item too");
    assert!(change.first <= change.last);
    assert_eq!(change.tables.get(&b"player"[..]), Some(&2));
    assert_eq!(change.tables.len(), 1);
    assert!(!change.main_moved);
    assert!(!change.conflicts);
}

#[test]
fn main_and_branches_without_their_own_commits_are_not_waiting() {
    let dir = TempDir::new("nothing");
    let mut store = open(&dir.log());
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/7/gems", "30")],
            by("ops:seed", "seed"),
        )
        .expect("commits");
    store
        .create_branch(AGENT_BRANCH, MAIN_BRANCH)
        .expect("branches");

    assert!(store.pending().expect("lists").is_empty());
}

#[test]
fn approving_merges_the_change_as_seen_and_records_the_approver() {
    let dir = TempDir::new("approve");
    let mut store = with_a_change(&dir.log());
    let head = store.head(AGENT_BRANCH).expect("has commits");

    let approved = store
        .approve(
            AGENT_BRANCH,
            &head,
            by("person:lead", "checked ticket 8812"),
        )
        .expect("approves");

    let Approved::Commit(id) = approved else {
        panic!("expected a merge, got {approved:?}");
    };
    assert_eq!(store.head(MAIN_BRANCH), Some(id));
    assert_eq!(
        store.get(MAIN_BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"530"[..]))
    );
    let merge = store.commit_info(&id).expect("recorded");
    assert_eq!(merge.provenance.who, "person:lead");
    assert_eq!(merge.provenance.why, "checked ticket 8812");
    assert_eq!(merge.provenance.approved_by.as_deref(), Some("person:lead"));
    assert!(store.pending().expect("lists").is_empty());
}

#[test]
fn a_change_written_to_after_it_was_seen_is_not_approved() {
    let dir = TempDir::new("moved");
    let mut store = with_a_change(&dir.log());
    let seen = store.head(AGENT_BRANCH).expect("has commits");
    store
        .commit(
            AGENT_BRANCH,
            vec![set("player/7/gems", "99999")],
            by("agent:support", "one more"),
        )
        .expect("commits");
    let main_before = store.head(MAIN_BRANCH);

    let refused = store.approve(AGENT_BRANCH, &seen, by("person:lead", "looked fine"));

    assert!(matches!(refused, Err(StoreError::NotWaiting(head)) if head == seen));
    assert_eq!(store.head(MAIN_BRANCH), main_before);
}

#[test]
fn a_conflict_is_flagged_and_approving_it_writes_nothing() {
    let dir = TempDir::new("conflict");
    let mut store = with_a_change(&dir.log());
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/7/gems", "31")],
            by("game:shop", "bought a potion"),
        )
        .expect("commits");
    let main_before = store.head(MAIN_BRANCH);

    let change = store.pending().expect("lists").remove(0);
    assert!(change.main_moved);
    assert!(change.conflicts);
    let approved = store
        .approve(&change.branch, &change.head, by("person:lead", "go"))
        .expect("answers");

    let Approved::Conflicts(conflicts) = approved else {
        panic!("expected conflicts, got {approved:?}");
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(&*conflicts[0].key, b"player/7/gems");
    assert_eq!(store.head(MAIN_BRANCH), main_before);
}

#[test]
fn a_rejected_change_leaves_the_list_and_stays_rejected_after_reopening() {
    let dir = TempDir::new("reject");
    let mut store = with_a_change(&dir.log());
    let head = store.head(AGENT_BRANCH).expect("has commits");
    let main_before = store.head(MAIN_BRANCH);

    store
        .reject(AGENT_BRANCH, &head, by("person:lead", "wrong player"))
        .expect("rejects");

    assert!(store.pending().expect("lists").is_empty());
    assert_eq!(store.head(MAIN_BRANCH), main_before);
    drop(store);
    let reopened = open(&dir.log());
    assert!(reopened.pending().expect("lists").is_empty());
    let rejection = reopened.rejection(AGENT_BRANCH).expect("recorded");
    assert_eq!(rejection.head, head);
    assert_eq!(rejection.provenance.who, "person:lead");
    assert_eq!(rejection.provenance.why, "wrong player");
}

#[test]
fn writing_to_a_rejected_branch_makes_a_new_change() {
    let dir = TempDir::new("again");
    let mut store = with_a_change(&dir.log());
    let head = store.head(AGENT_BRANCH).expect("has commits");
    store
        .reject(AGENT_BRANCH, &head, by("person:lead", "wrong player"))
        .expect("rejects");

    store
        .commit(
            AGENT_BRANCH,
            vec![set("player/8/gems", "530")],
            by("agent:support", "the right player"),
        )
        .expect("commits");

    let pending = store.pending().expect("lists");
    assert_eq!(pending.len(), 1);
    assert_ne!(pending[0].head, head);
}

#[test]
fn a_head_no_change_has_is_refused_for_reject_too() {
    let dir = TempDir::new("unknown");
    let mut store = with_a_change(&dir.log());

    let refused = store.reject(AGENT_BRANCH, &[9; 32], by("person:lead", "?"));

    assert!(matches!(refused, Err(StoreError::NotWaiting(_))));
    assert_eq!(store.pending().expect("lists").len(), 1);
}

/// A branch made from another starts at the same head; rejecting it must
/// reject it, not the branch it came from.
#[test]
fn two_branches_at_one_head_are_told_apart_by_name() {
    let dir = TempDir::new("same-head");
    let mut store = with_a_change(&dir.log());
    store
        .create_branch("agent/copy", AGENT_BRANCH)
        .expect("branches");
    let head = store.head(AGENT_BRANCH).expect("has commits");

    store
        .reject("agent/copy", &head, by("person:lead", "a copy"))
        .expect("rejects");

    let waiting: Vec<String> = store
        .pending()
        .expect("lists")
        .into_iter()
        .map(|change| change.branch)
        .collect();
    assert_eq!(waiting, vec![AGENT_BRANCH.to_owned()]);
    assert!(store.rejection(AGENT_BRANCH).is_none());
}
