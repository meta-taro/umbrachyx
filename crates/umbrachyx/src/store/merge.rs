//! Diffs between commits, three-way merges, and undoing one commit.
//!
//! All three work on values: a cell rewritten with the value it already had
//! is not a change. Each reads the tree diff, so its cost follows the number
//! of changed keys, not the size of the data.
//!
//! A conflict is reported, never resolved: when both sides changed a key to
//! different values, or a key to be undone has changed since, nothing is
//! written and the caller gets every conflicting key with its values.

use std::collections::{BTreeMap, HashSet, VecDeque};

use super::{Clock, CommitId, Provenance, Storage, Store, StoreError, decode_cell};
use crate::log::Edit;
use crate::tree::{Bytes, Change, Tree};

/// A key whose value differs between two commits. `None` means absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChange {
    pub key: Bytes,
    pub before: Option<Bytes>,
    pub after: Option<Bytes>,
}

/// A key the two sides disagree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub key: Bytes,
    /// Where both sides started.
    pub base: Option<Bytes>,
    /// The branch being written to.
    pub ours: Option<Bytes>,
    /// What the merge or the undo would have written.
    pub theirs: Option<Bytes>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    Commit(CommitId),
    /// Everything on the other branch is already here.
    UpToDate,
    /// Nothing was written.
    Conflicts(Vec<Conflict>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reverted {
    Commit(CommitId),
    /// Nothing was written.
    Conflicts(Vec<Conflict>),
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// What changed from `from` to `to`; `None` stands for the empty state
    /// before the first commit. Sorted by key.
    pub fn diff(
        &self,
        from: Option<&CommitId>,
        to: Option<&CommitId>,
    ) -> Result<Vec<ValueChange>, StoreError> {
        Ok(value_changes(&self.state_at(from)?, &self.state_at(to)?))
    }

    /// Brings into `into` what `from` changed since they last shared a
    /// commit. Changes to different keys combine; the same change on both
    /// sides is taken once; different changes to one key are conflicts.
    pub fn merge(
        &mut self,
        into: &str,
        from: &str,
        provenance: Provenance,
    ) -> Result<Merged, StoreError> {
        self.require_branch(into)?;
        self.require_branch(from)?;
        let ours = self.head(into);
        let Some(theirs) = self.head(from) else {
            return Ok(Merged::UpToDate);
        };
        if ours.is_some_and(|ours| self.ancestors(ours).contains(&theirs)) {
            return Ok(Merged::UpToDate);
        }
        let base = ours.and_then(|ours| self.merge_base(ours, theirs));
        let ours_changed: BTreeMap<Bytes, Option<Bytes>> = self
            .diff(base.as_ref(), ours.as_ref())?
            .into_iter()
            .map(|change| (change.key, change.after))
            .collect();
        let mut edits = Vec::new();
        let mut conflicts = Vec::new();
        for change in self.diff(base.as_ref(), Some(&theirs))? {
            match ours_changed.get(&change.key) {
                None => edits.push(edit_to(&change.key, change.after.as_ref())),
                Some(ours) if *ours == change.after => {}
                Some(ours) => conflicts.push(Conflict {
                    key: change.key,
                    base: change.before,
                    ours: ours.clone(),
                    theirs: change.after,
                }),
            }
        }
        if !conflicts.is_empty() {
            return Ok(Merged::Conflicts(conflicts));
        }
        let id = self.commit_with(into, edits, provenance, Some(theirs))?;
        Ok(Merged::Commit(id))
    }

    /// Writes to `branch` the opposite of what `commit` changed, if none of
    /// those keys changed since. Other keys are left as they are.
    pub fn revert(
        &mut self,
        branch: &str,
        commit: &CommitId,
        provenance: Provenance,
    ) -> Result<Reverted, StoreError> {
        self.require_branch(branch)?;
        let parent = self
            .commits
            .get(commit)
            .ok_or(StoreError::UnknownCommit(*commit))?
            .parent;
        let is_on_branch = self
            .head(branch)
            .is_some_and(|head| self.ancestors(head).contains(commit));
        if !is_on_branch {
            return Err(StoreError::NotOnBranch(*commit));
        }
        let current = &self.branches[branch].state;
        let mut edits = Vec::new();
        let mut conflicts = Vec::new();
        for change in self.diff(parent.as_ref(), Some(commit))? {
            let now = live_value(current, &change.key);
            if now == change.after {
                edits.push(edit_to(&change.key, change.before.as_ref()));
            } else {
                conflicts.push(Conflict {
                    key: change.key,
                    base: change.after,
                    ours: now,
                    theirs: change.before,
                });
            }
        }
        if !conflicts.is_empty() {
            return Ok(Reverted::Conflicts(conflicts));
        }
        let id = self.commit_with(branch, edits, provenance, None)?;
        Ok(Reverted::Commit(id))
    }

    fn state_at(&self, commit: Option<&CommitId>) -> Result<Tree, StoreError> {
        match commit {
            None => Ok(Tree::empty()),
            Some(id) => self
                .commits
                .get(id)
                .map(|info| info.state.clone())
                .ok_or(StoreError::UnknownCommit(*id)),
        }
    }

    /// `id` and every commit before it, through both parents of merges.
    fn ancestors(&self, id: CommitId) -> HashSet<CommitId> {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([id]);
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(info) = self.commits.get(&id) {
                queue.extend(info.parent);
                queue.extend(info.merged);
            }
        }
        seen
    }

    /// The best commit both histories share: one that is not an ancestor of
    /// another shared commit. Walking back from one side and taking the
    /// first shared commit found is not enough: after the other side merged
    /// this one in, an older shared commit can be as near as the right one.
    /// With criss-cross merges several can qualify; this takes the latest.
    ///
    /// The cost grows with the shared history; it is not yet bounded.
    fn merge_base(&self, ours: CommitId, theirs: CommitId) -> Option<CommitId> {
        let ours = self.ancestors(ours);
        let shared: HashSet<CommitId> = self
            .ancestors(theirs)
            .into_iter()
            .filter(|id| ours.contains(id))
            .collect();
        let mut superseded = HashSet::new();
        for id in &shared {
            if superseded.contains(id) {
                continue;
            }
            superseded.extend(self.ancestors(*id).into_iter().filter(|a| a != id));
        }
        shared
            .into_iter()
            .filter(|id| !superseded.contains(id))
            .max_by_key(|id| (self.commits.get(id).map_or(0, |info| info.time), *id))
    }
}

/// Value changes between two states of cells, skipping cells rewritten
/// with the same value and removals of what was already removed.
fn value_changes(from: &Tree, to: &Tree) -> Vec<ValueChange> {
    from.diff(to)
        .changes
        .into_iter()
        .filter_map(|change| {
            let (key, before, after) = match change {
                Change::Added { key, value } => (key, None, decode_cell(&value).value),
                Change::Removed { key, value } => (key, decode_cell(&value).value, None),
                Change::Modified { key, old, new } => {
                    (key, decode_cell(&old).value, decode_cell(&new).value)
                }
            };
            (before != after).then_some(ValueChange { key, before, after })
        })
        .collect()
}

fn live_value(state: &Tree, key: &[u8]) -> Option<Bytes> {
    decode_cell(state.get(key)?).value
}

fn edit_to(key: &Bytes, value: Option<&Bytes>) -> Edit {
    Edit {
        key: key.to_vec(),
        value: value.map(|v| v.to_vec()),
    }
}
