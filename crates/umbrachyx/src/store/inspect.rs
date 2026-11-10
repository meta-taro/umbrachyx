//! What can be read of a store's history, whatever its storage and clock.
//!
//! The committer owns its store on its own thread, so a question about the
//! history goes to that thread as a closure over [`Inspect`], and is
//! answered there between groups of commits (see `Writer::ask`). The trait
//! names only reads, so a question cannot write.

use super::{Cell, Clock, CommitId, CommitInfo, Store, StoreError, ValueChange, decode_cell};
use crate::log::{Engine, Storage};
use crate::tree::Bytes;

/// The reads of [`Store`] that do not depend on its storage or clock.
pub trait Inspect {
    fn branches(&self) -> Vec<&str>;
    fn head(&self, branch: &str) -> Option<CommitId>;
    fn commit_info(&self, id: &CommitId) -> Option<&CommitInfo>;
    /// The newest `limit` commits of [`Store::log`].
    fn log(&self, branch: &str, limit: usize) -> Vec<&CommitInfo>;
    fn resolve(&self, name: &str) -> Result<CommitId, StoreError>;
    /// The newest `limit` changes of [`Store::history`].
    fn history(&self, branch: &str, key: &[u8], limit: usize) -> Vec<Cell>;
    /// `None` stands for no commit at all: a branch nothing was written to.
    fn diff(
        &self,
        from: Option<&CommitId>,
        to: Option<&CommitId>,
    ) -> Result<Vec<ValueChange>, StoreError>;
    fn cells(&self, branch: &str, after: Option<&[u8]>, limit: usize) -> Vec<(Bytes, Cell)>;
    /// Every declared table and its engine ([`Store::tables`]).
    fn tables(&self) -> Vec<(Vec<u8>, Engine)>;
    /// [`Store::latest_table_of`].
    fn latest_table_of<'k>(&self, key: &'k [u8]) -> Option<&'k [u8]>;
    /// [`Store::latest_tables`].
    fn latest_tables(&self) -> Vec<Vec<u8>>;
    /// [`Store::omitted_tables`].
    fn omitted_tables(&self, from: Option<&CommitId>, to: Option<&CommitId>) -> Vec<Vec<u8>>;
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// The cells of `branch` in key order, starting after `after`, at most
    /// `limit` of them. A removed key is left out; an erased one is kept,
    /// marked so. A page costs its own cells and one path down the tree.
    /// Keys of latest-value tables, which keep no history, are not here.
    pub fn cells(&self, branch: &str, after: Option<&[u8]>, limit: usize) -> Vec<(Bytes, Cell)> {
        let Some(head) = self.branches.get(branch) else {
            return Vec::new();
        };
        let entries = match after {
            Some(after) => head.state.iter_after(after),
            None => head.state.iter(),
        };
        entries
            .map(|(key, bytes)| (key, decode_cell(bytes)))
            .filter(|(_, cell)| cell.value.is_some())
            .take(limit)
            .map(|(key, cell)| (key.clone(), self.revealed(key, cell)))
            .collect()
    }
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Stops after `limit` changes, so a key changed many times costs a
    /// page, not its whole history.
    /// A latest-value table keeps no history: its keys have none.
    pub(super) fn history_up_to(&self, branch: &str, key: &[u8], limit: usize) -> Vec<Cell> {
        let mut history = Vec::new();
        if self.is_latest(key) {
            return history;
        }
        let mut cell = self.raw_cell(branch, key);
        while let Some(current) = cell.filter(|_| history.len() < limit) {
            cell = self
                .commits
                .get(&current.changed_by)
                .and_then(|commit| commit.written(key)?.replaced.as_ref())
                .map(|bytes| decode_cell(bytes));
            history.push(self.revealed(key, current));
        }
        history
    }

    /// Stops after `limit` commits, so a long branch costs a page.
    pub(super) fn log_up_to(&self, branch: &str, limit: usize) -> Vec<&CommitInfo> {
        let mut commits = Vec::new();
        let mut next = self.head(branch);
        while let Some(info) = next.and_then(|id| self.commits.get(&id)) {
            if commits.len() == limit {
                break;
            }
            commits.push(info.as_ref());
            next = info.parent;
        }
        commits
    }
}

impl<S: Storage, C: Clock> Inspect for Store<S, C> {
    fn branches(&self) -> Vec<&str> {
        Store::branches(self).collect()
    }

    fn head(&self, branch: &str) -> Option<CommitId> {
        Store::head(self, branch)
    }

    fn commit_info(&self, id: &CommitId) -> Option<&CommitInfo> {
        Store::commit_info(self, id)
    }

    fn log(&self, branch: &str, limit: usize) -> Vec<&CommitInfo> {
        self.log_up_to(branch, limit)
    }

    fn resolve(&self, name: &str) -> Result<CommitId, StoreError> {
        Store::resolve(self, name)
    }

    fn history(&self, branch: &str, key: &[u8], limit: usize) -> Vec<Cell> {
        self.history_up_to(branch, key, limit)
    }

    fn diff(
        &self,
        from: Option<&CommitId>,
        to: Option<&CommitId>,
    ) -> Result<Vec<ValueChange>, StoreError> {
        Store::diff(self, from, to)
    }

    fn cells(&self, branch: &str, after: Option<&[u8]>, limit: usize) -> Vec<(Bytes, Cell)> {
        Store::cells(self, branch, after, limit)
    }

    fn tables(&self) -> Vec<(Vec<u8>, Engine)> {
        Store::tables(self)
            .iter()
            .map(|(name, engine)| (name.clone(), *engine))
            .collect()
    }

    fn latest_table_of<'k>(&self, key: &'k [u8]) -> Option<&'k [u8]> {
        Store::latest_table_of(self, key)
    }

    fn latest_tables(&self) -> Vec<Vec<u8>> {
        Store::latest_tables(self)
    }

    fn omitted_tables(&self, from: Option<&CommitId>, to: Option<&CommitId>) -> Vec<Vec<u8>> {
        Store::omitted_tables(self, from, to)
    }
}
