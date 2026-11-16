//! Puts answered before they are on disk: the writes of plain-async
//! tables that came through a committer.
//!
//! They are in the map, so every reader sees them, and held here until
//! the log has them. They reach the log in the order they were answered
//! and ahead of anything written after them: the next record the store
//! writes for any reason starts with them, in the same frame when it
//! fits. So what a power cut leaves is everything answered up to some
//! moment and nothing after it; a write that waited for the disk never
//! survives without the puts answered before it.
//!
//! What is held is bounded by [`MAX_UNSYNCED_LEN`]. A put that would pass
//! it waits for the disk like any other write, and takes the held ones
//! with it.
//!
//! If the log then fails to take them, memory is ahead of the disk for
//! good: the log refuses every later write, and so does this, rather
//! than go on answering writes that nothing will keep.

use super::{Clock, Store, StoreError};
use crate::log::record::edits_len;
use crate::log::{Edit, LogError, Record, Storage};

/// The most bytes of puts held in memory and not yet in the log. Small
/// beside a record's limit, so held puts nearly always fit the frame of
/// the write that carries them.
pub(super) const MAX_UNSYNCED_LEN: usize = 4 * 1024 * 1024;

/// Puts applied to the map and not yet in the log, oldest first.
#[derive(Debug, Default)]
pub(super) struct Unsynced {
    pub(super) edits: Vec<Edit>,
    /// What the edits add to a record's payload, in bytes.
    pub(super) len: usize,
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Whether puts were answered that the log does not hold yet.
    pub fn has_unsynced_puts(&self) -> bool {
        !self.unsynced.edits.is_empty()
    }

    /// Writes the puts answered and not yet in the log, as one record,
    /// and returns once they are durable. Nothing held writes nothing.
    pub fn sync_puts(&mut self) -> Result<(), StoreError> {
        if self.unsynced.edits.is_empty() {
            return Ok(());
        }
        let held = std::mem::take(&mut self.unsynced);
        self.log.append(&Record::Puts(held.edits))?;
        Ok(())
    }

    /// Applies `puts`, already sealed, and holds them for the log, if
    /// they fit under the bound. Gives them back if they do not: they
    /// are then written like any other.
    pub(super) fn hold(&mut self, puts: Vec<Edit>) -> Result<Option<Vec<Edit>>, StoreError> {
        if self.log.is_poisoned() {
            return Err(LogError::Poisoned.into());
        }
        let len = edits_len(&puts);
        if self.unsynced.len + len > MAX_UNSYNCED_LEN {
            return Ok(Some(puts));
        }
        self.apply_puts(&puts);
        self.unsynced.len += len;
        self.unsynced.edits.extend(puts);
        Ok(None)
    }

    /// Whether every key is in a table whose writes may be answered
    /// before they are on disk.
    pub(super) fn may_answer_early(&self, edits: &[Edit]) -> bool {
        edits
            .iter()
            .all(|edit| self.tables.engine_of(&edit.key) == crate::log::Engine::PlainAsync)
    }
}
