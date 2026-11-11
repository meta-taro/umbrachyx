//! The log as JSON Lines: one record a line, in the order the log holds
//! them, for tools such as `jq` and `grep`. The log itself stays binary;
//! this is a way to read it, not another form of it.
//!
//! Commits written together by one sync are a line each. A value is shown
//! as [`crate::report`] shows values; one whose subject was erased is
//! `null` with `"erased": true`, and a removal is `null` without it.

use serde_json::{Value, json};

use super::seal::{Ring, Sealer};
use super::{Clock, CommitId, Replayed, Store, StoreError, hex};
use crate::keys::{KeyStorage, Keys};
use crate::log::{Commit, Edit, Record, Storage};
use crate::report::shown;

impl<S: Storage, C: Clock> Store<S, C> {
    /// Every record of the log as a line of JSON, values opened with
    /// `keys`. Opens the log as [`Store::open_sealed`] does, so it finishes
    /// what a crash cut short before it reads, and is refused like any
    /// other opening while a server holds the log.
    pub fn export(
        storage: S,
        keys: Keys<Box<dyn KeyStorage + Send>>,
        clock: C,
    ) -> Result<Vec<String>, StoreError> {
        let sealer = Sealer::new(keys);
        let ring = sealer.ring();
        let (store, replayed) = Self::replay(storage, clock, Some(sealer), true)?;
        Ok(replayed
            .iter()
            .flat_map(|record| store.lines(record, &ring))
            .map(|line| line.to_string())
            .collect())
    }

    fn lines(&self, (record, ids): &Replayed, ring: &Ring) -> Vec<Value> {
        match record {
            Record::Commit(commit) => ids
                .iter()
                .map(|id| self.commit_line(commit, id, ring))
                .collect(),
            Record::Group(commits) => commits
                .iter()
                .zip(ids)
                .map(|(commit, id)| self.commit_line(commit, id, ring))
                .collect(),
            Record::Branch(branch) => {
                vec![json!({"record": "branch", "name": branch.name, "from": branch.from})]
            }
            Record::Erase(erase) => vec![json!({
                "record": "erase",
                "subject": shown(Some(&erase.subject)),
                "time_us": erase.time,
                "who": erase.who,
                "why": erase.why,
                "approved_by": approvers(erase.approved_by.as_ref()),
            })],
            Record::Table(table) => vec![json!({
                "record": "table",
                "name": shown(Some(&table.name)),
                "engine": table.engine.name(),
                "time_us": table.time,
                "who": table.who,
                "why": table.why,
                "approved_by": approvers(table.approved_by.as_ref()),
            })],
            Record::Reject(reject) => vec![json!({
                "record": "reject",
                "branch": reject.branch,
                "head": hex(&reject.head),
                "time_us": reject.time,
                "who": reject.who,
                "why": reject.why,
            })],
        }
    }

    fn commit_line(&self, commit: &Commit, id: &CommitId, ring: &Ring) -> Value {
        let parent = self.commit_info(id).and_then(|info| info.parent);
        let changes: Vec<Value> = commit
            .changes
            .iter()
            .map(|edit| change(edit, ring))
            .collect();
        json!({
            "record": "commit",
            "commit": hex(id),
            "parent": parent.as_ref().map(hex),
            "merged": commit.merged.as_ref().map(hex),
            "branch": commit.branch,
            "time_us": commit.time,
            "who": commit.who,
            "why": commit.why,
            "approved_by": approvers(commit.approved_by.as_ref()),
            "changes": changes,
        })
    }
}

/// One key set or removed. A value that no longer opens was erased.
fn change(edit: &Edit, ring: &Ring) -> Value {
    let opened = edit
        .value
        .as_ref()
        .map(|sealed| super::seal::open(ring, &edit.key, sealed));
    json!({
        "key": shown(Some(&edit.key)),
        "value": shown(opened.as_ref().and_then(|value| value.as_deref())),
        "erased": matches!(opened, Some(None)),
    })
}

/// Who approved, as a list, as [`crate::report`] writes it.
fn approvers(approved_by: Option<&String>) -> Vec<&String> {
    approved_by.into_iter().collect()
}
