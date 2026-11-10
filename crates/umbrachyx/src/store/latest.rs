//! What a latest-value table can say about change: diffs it can still
//! compute, and three-way merges over the keys a branch wrote.
//!
//! On `main` such a table keeps only the current cell of each key, so the
//! past of `main` cannot be diffed for it; a diff that would need it leaves
//! those tables out and says so (`omitted_tables`). Off `main`, each commit
//! keeps the cells it wrote and the cells they replaced, so a merge knows,
//! for every key a branch changed, the value it started from.

use std::collections::{HashMap, HashSet};

use super::merge::{Conflict, ValueChange, edit_to};
use super::{Clock, CommitId, MAIN_BRANCH, Storage, Store, Written, decode_cell};
use crate::hamt::Hamt;
use crate::log::{Edit, Engine};
use crate::tree::{Bytes, Change};

/// For each key a line of commits wrote, the cell it replaced the first
/// time: what the key held where the line began.
type Started = HashMap<Bytes, Option<Bytes>>;

impl<S: Storage, C: Clock> Store<S, C> {
    pub(super) fn is_latest(&self, key: &[u8]) -> bool {
        self.tables.engine_of(key) == Engine::Latest
    }

    /// The latest-value tables a diff from `from` to `to` leaves out,
    /// because `main` keeps no past for them. Empty when the diff is whole.
    pub fn omitted_tables(&self, from: Option<&CommitId>, to: Option<&CommitId>) -> Vec<Vec<u8>> {
        if self.latest_changes(from, to).is_some() {
            return Vec::new();
        }
        self.tables
            .declared()
            .iter()
            .filter(|(_, engine)| **engine == Engine::Latest)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The changes to latest-value tables from `from` to `to`, values
    /// sealed, or `None` when they cannot be known: between two branch
    /// heads, from the maps themselves; for a commit off `main` against its
    /// parent, from the cells it kept.
    pub(super) fn latest_changes(
        &self,
        from: Option<&CommitId>,
        to: Option<&CommitId>,
    ) -> Option<Vec<ValueChange>> {
        if !self
            .tables
            .declared()
            .values()
            .any(|e| *e == Engine::Latest)
        {
            return Some(Vec::new());
        }
        if let (Some(old), Some(new)) = (self.latest_at(from), self.latest_at(to)) {
            return Some(map_changes(&old, &new));
        }
        let info = self.commits.get(to?)?;
        let is_parent = from.is_some_and(|from| info.parent == Some(*from));
        (is_parent && info.branch != MAIN_BRANCH).then(|| written_changes(&info.latest_written))
    }

    /// The latest-value map at `commit`, known only at a branch head.
    /// `None` for no commit at all is the empty map.
    fn latest_at(&self, commit: Option<&CommitId>) -> Option<Hamt> {
        match commit {
            None => Some(Hamt::empty()),
            Some(id) => self.head_of(id).map(|head| head.latest.clone()),
        }
    }

    /// The edits that bring into `into` what `from` changed in latest-value
    /// tables, and the keys both changed differently. Only keys that differ
    /// between the two heads now are looked at. A side off `main` knows what
    /// it changed and where each key started; when the other side is
    /// `main`, a differing key the branch did not change was changed on
    /// `main`.
    pub(super) fn merge_latest(&self, into: &str, from: &str) -> (Vec<Edit>, Vec<Conflict>) {
        let (ours, theirs) = (&self.branches[into], &self.branches[from]);
        let ours_started = self.started(into, ours.commit, theirs.commit);
        let theirs_started = self.started(from, theirs.commit, ours.commit);
        let mut edits = Vec::new();
        let mut conflicts = Vec::new();
        for change in map_changes(&ours.latest, &theirs.latest) {
            let ValueChange {
                key,
                before: now_ours,
                after: now_theirs,
                ..
            } = change;
            let base = match (&theirs_started, &ours_started) {
                (Some(started), _) => match started.get(&key) {
                    Some(cell) => value_of(cell.as_ref()),
                    // They left it alone, so the difference is ours.
                    None => continue,
                },
                (None, Some(started)) => match started.get(&key) {
                    Some(cell) => value_of(cell.as_ref()),
                    // We left it alone, so `main` changed it.
                    None => now_ours.clone(),
                },
                (None, None) => continue,
            };
            if now_ours == base {
                edits.push(edit_to(&key, now_theirs.as_ref()));
            } else if now_theirs != base {
                conflicts.push(Conflict {
                    key,
                    base,
                    ours: now_ours,
                    theirs: now_theirs,
                });
            }
        }
        (edits, conflicts)
    }

    /// Where each latest-value key `branch` wrote started, over its own
    /// line back from `head` to the first commit `other` also has. A key a
    /// merge commit wrote starts at the value the merge gave it: both sides
    /// agreed on it there, whatever came before. `None` for `main`, which
    /// keeps no such cells.
    fn started(
        &self,
        branch: &str,
        head: Option<CommitId>,
        other: Option<CommitId>,
    ) -> Option<Started> {
        if branch == MAIN_BRANCH {
            return None;
        }
        let shared: HashSet<CommitId> = other.map(|id| self.ancestors(id)).unwrap_or_default();
        let mut started = Started::new();
        // Keys a merge settled; older writes no longer say where they began.
        let mut settled: HashSet<Bytes> = HashSet::new();
        let mut next = head;
        while let Some(id) = next.filter(|id| !shared.contains(id)) {
            let Some(info) = self.commits.get(&id) else {
                break;
            };
            // Walking back, the oldest write of a key is seen last.
            for written in &info.latest_written {
                if settled.contains(&written.key) {
                    continue;
                }
                if info.merged.is_some() {
                    settled.insert(written.key.clone());
                    started.insert(written.key.clone(), Some(written.cell.clone()));
                } else {
                    started.insert(written.key.clone(), written.replaced.clone());
                }
            }
            next = info.parent;
        }
        Some(started)
    }
}

fn value_of(cell: Option<&Bytes>) -> Option<Bytes> {
    decode_cell(cell?).value
}

/// Value changes between two latest-value maps. A map keeps no removed
/// key, so a removal is a key gone.
fn map_changes(from: &Hamt, to: &Hamt) -> Vec<ValueChange> {
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
            (before != after).then_some(ValueChange {
                key,
                before,
                after,
                erased: false,
            })
        })
        .collect()
}

fn written_changes(written: &[Written]) -> Vec<ValueChange> {
    written
        .iter()
        .filter_map(|written| {
            let before = value_of(written.replaced.as_ref());
            let after = decode_cell(&written.cell).value;
            (before != after).then(|| ValueChange {
                key: written.key.clone(),
                before,
                after,
                erased: false,
            })
        })
        .collect()
}
