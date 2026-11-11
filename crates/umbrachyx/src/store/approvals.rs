//! Changes waiting on branches, and approving or rejecting them.
//!
//! A change is a branch whose head `main` does not have and that was not
//! rejected at that head: the branch's own commits, back to the first one
//! `main` has. A person approves or rejects it by its branch and the head
//! they saw, so anything written to the branch after that is a different
//! change, never merged unseen. The head alone would not do: a branch made
//! from another starts at the same head, and is a change of its own. Who may approve is decided before the store is asked,
//! by the caller's capability token.

use std::collections::{BTreeMap, HashSet};

use super::merge::MergePlan;
use super::{
    Clock, CommitId, Conflict, Decision, MAIN_BRANCH, NotWaiting, Provenance, Rejection, Storage,
    Store, StoreError,
};
use crate::log::{Record, Reject};
use crate::tree::Bytes;

/// One change waiting, as a list of changes shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    pub branch: String,
    /// The commit a person approves or rejects.
    pub head: CommitId,
    /// The latest commit the branch shares with `main`: where it started,
    /// or where it last took `main` in. The diff from here to `head` is
    /// what the branch changed, and nothing `main` changed since. `None`
    /// when they share no commit.
    pub base: Option<CommitId>,
    /// Who wrote the change's commits, oldest first, each once.
    pub authors: Vec<String>,
    /// Why, from the latest commit.
    pub why: String,
    /// When the first and the latest commit of the change were made.
    pub first: u64,
    pub last: u64,
    /// How many keys change, per table (the first part of the key).
    pub tables: BTreeMap<Vec<u8>, usize>,
    /// Whether `main` has commits the branch did not start from.
    pub main_moved: bool,
    /// Whether merging it now would stop on keys both sides changed.
    pub conflicts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approved {
    /// The merge into `main`.
    Commit(CommitId),
    /// Nothing was written.
    Conflicts(Vec<Conflict>),
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Every change waiting, by branch name.
    pub fn pending(&self) -> Result<Vec<PendingChange>, StoreError> {
        let main = self.head(MAIN_BRANCH);
        let on_main = main.map(|main| self.ancestors(main)).unwrap_or_default();
        let mut changes = Vec::new();
        for branch in self.branches.keys() {
            if let Some(change) = self.change_on(branch, main, &on_main)? {
                changes.push(change);
            }
        }
        Ok(changes)
    }

    /// Merges the change waiting on `branch` into `main`, if its head is
    /// still `head`, recorded as made and approved by `provenance.who`.
    pub fn approve(
        &mut self,
        branch: &str,
        head: &CommitId,
        provenance: Provenance,
    ) -> Result<Approved, StoreError> {
        self.require_waiting(branch, head)?;
        match self.merge_plan(MAIN_BRANCH, branch)? {
            MergePlan::UpToDate => Err(StoreError::NotWaiting(*head, NotWaiting::Decided)),
            MergePlan::Conflicts(conflicts) => Ok(Approved::Conflicts(
                conflicts
                    .into_iter()
                    .map(|c| self.revealed_conflict(c))
                    .collect(),
            )),
            MergePlan::Edits { edits, theirs } => {
                let provenance = Provenance {
                    approved_by: Some(provenance.who.clone()),
                    ..provenance
                };
                let id = self.commit_with(MAIN_BRANCH, edits, provenance, Some(theirs))?;
                Ok(Approved::Commit(id))
            }
        }
    }

    /// Closes the change waiting on `branch`, if its head is still `head`,
    /// without merging it, with who and why kept in the log.
    pub fn reject(
        &mut self,
        branch: &str,
        head: &CommitId,
        provenance: Provenance,
    ) -> Result<(), StoreError> {
        self.require_waiting(branch, head)?;
        let time = self.next_time();
        let record = Record::Reject(Reject {
            branch: branch.to_owned(),
            head: *head,
            who: provenance.who,
            why: provenance.why,
            time,
        });
        self.log.append(&record)?;
        self.apply(&record).map(drop)
    }

    /// The keys the change waiting on `branch` at `head` would change in
    /// `main`, so a caller can check each against who may approve it.
    pub fn change_keys(&self, branch: &str, head: &CommitId) -> Result<Vec<Bytes>, StoreError> {
        self.require_waiting(branch, head)?;
        let base = self
            .head(MAIN_BRANCH)
            .and_then(|main| self.merge_base(main, *head));
        Ok(self
            .diff(base.as_ref(), Some(head))?
            .into_iter()
            .map(|change| change.key)
            .collect())
    }

    /// The newest `limit` approvals and rejections, newest first.
    pub fn decisions(&self, limit: usize) -> Vec<&Decision> {
        self.decisions.iter().rev().take(limit).collect()
    }

    /// The last change rejected on `branch`.
    pub fn rejection(&self, branch: &str) -> Option<&Rejection> {
        self.rejections.get(branch)
    }

    /// That `branch` has a change waiting and its head is `head`, or why
    /// not: decided, moved on since, or never there.
    fn require_waiting(&self, branch: &str, head: &CommitId) -> Result<(), StoreError> {
        self.require_branch(branch)?;
        let main = self.head(MAIN_BRANCH);
        let on_main = main.map(|main| self.ancestors(main)).unwrap_or_default();
        let current = self.head(branch);
        let rejected_here = self
            .rejections
            .get(branch)
            .is_some_and(|rejection| rejection.head == *head);
        let why = if on_main.contains(head) || rejected_here {
            NotWaiting::Decided
        } else if branch == MAIN_BRANCH {
            NotWaiting::None
        } else if current == Some(*head) {
            return Ok(());
        } else if current.is_some_and(|current| self.ancestors(current).contains(head)) {
            NotWaiting::Moved
        } else {
            NotWaiting::None
        };
        Err(StoreError::NotWaiting(*head, why))
    }

    fn is_waiting(&self, branch: &str, head: &CommitId, on_main: &HashSet<CommitId>) -> bool {
        !on_main.contains(head)
            && !self
                .rejections
                .get(branch)
                .is_some_and(|rejection| rejection.head == *head)
    }

    fn change_on(
        &self,
        branch: &str,
        main: Option<CommitId>,
        on_main: &HashSet<CommitId>,
    ) -> Result<Option<PendingChange>, StoreError> {
        if branch == MAIN_BRANCH {
            return Ok(None);
        }
        let Some(head) = self.head(branch) else {
            return Ok(None);
        };
        if !self.is_waiting(branch, &head, on_main) {
            return Ok(None);
        }
        let own: Vec<_> = self
            .log(branch)
            .into_iter()
            .take_while(|commit| !on_main.contains(&commit.id))
            .collect();
        let (Some(newest), Some(oldest)) = (own.first(), own.last()) else {
            return Ok(None);
        };
        let mut authors: Vec<String> = Vec::new();
        for commit in own.iter().rev() {
            if !authors.contains(&commit.provenance.who) {
                authors.push(commit.provenance.who.clone());
            }
        }
        let base = main.and_then(|main| self.merge_base(main, head));
        let mut tables = BTreeMap::new();
        for change in self.diff(base.as_ref(), Some(&head))? {
            *tables.entry(first_part(&change.key).to_vec()).or_insert(0) += 1;
        }
        Ok(Some(PendingChange {
            branch: branch.to_owned(),
            head,
            base,
            authors,
            why: newest.provenance.why.clone(),
            first: oldest.time,
            last: newest.time,
            tables,
            main_moved: main != base,
            conflicts: matches!(
                self.merge_plan(MAIN_BRANCH, branch)?,
                MergePlan::Conflicts(_)
            ),
        }))
    }
}

/// The table a key is in: its part before the first `/`, or all of it.
fn first_part(key: &[u8]) -> &[u8] {
    key.split(|byte| *byte == b'/').next().unwrap_or(key)
}
