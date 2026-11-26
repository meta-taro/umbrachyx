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
//! While the store writes them, which takes as long as the disk does,
//! it can take no more: the puts that come meanwhile are held in an
//! [`Aside`] by whoever serves, applied to the map as the others are,
//! and taken in when the write ends, behind the ones just written and
//! ahead of anything else.
//!
//! If the log then fails to take them, memory is ahead of the disk for
//! good: the log refuses every later write, and so does this, rather
//! than go on answering writes that nothing will keep.

use std::collections::HashMap;
use std::sync::Arc;

use super::plain::PlainMap;
use super::tables::table_of;
use super::{Clock, Store, StoreError};
use crate::log::record::edits_len;
use crate::log::{Edit, Engine, LogError, Record, Storage};

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
    /// Whether the log refused a record that held answered puts: they
    /// are in memory and will never be on disk.
    pub(super) is_lost: bool,
}

/// Puts answered while the store was writing the ones before them. They
/// are in the map, so every reader sees them; the store takes them in,
/// in the order they came, when its write ends.
#[derive(Debug)]
pub(crate) struct Aside {
    plain: Arc<PlainMap>,
    tables: Arc<HashMap<Vec<u8>, Engine>>,
    edits: Vec<Edit>,
    len: usize,
}

impl Aside {
    /// Applies `puts`, sealed already, and holds them. Gives them back
    /// if one is not to a plain-async table, or if they would pass what
    /// may be held ahead of the disk.
    pub(crate) fn hold(&mut self, puts: Vec<Edit>) -> Option<Vec<Edit>> {
        let is_early = |edit: &Edit| {
            table_of(&edit.key).and_then(|table| self.tables.get(table))
                == Some(&Engine::PlainAsync)
        };
        let len = edits_len(&puts);
        if self.len + len > MAX_UNSYNCED_LEN || !puts.iter().all(is_early) {
            return Some(puts);
        }
        for edit in &puts {
            self.plain.put(&edit.key, edit.value.as_deref());
        }
        self.len += len;
        self.edits.extend(puts);
        None
    }
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Somewhere to hold the puts that come while this store writes, or
    /// `None` if it takes no writes of its own or its log takes no more.
    pub(crate) fn aside(&self) -> Option<Aside> {
        if self.current_only || self.log.is_poisoned() {
            return None;
        }
        Some(Aside {
            plain: Arc::clone(&self.plain),
            tables: self.tables.plain(),
            edits: Vec::new(),
            len: 0,
        })
    }

    /// Holds for the log the puts `aside` took while this store wrote.
    /// They are in the map already.
    pub(crate) fn take_in(&mut self, aside: Aside) {
        for edit in &aside.edits {
            self.tables.note_written(&edit.key);
        }
        self.unsynced.len += aside.len;
        self.unsynced.edits.extend(aside.edits);
    }

    /// Whether puts were answered that the log does not hold yet.
    pub fn has_unsynced_puts(&self) -> bool {
        !self.unsynced.edits.is_empty()
    }

    /// Whether puts were answered that the log then refused. Readers
    /// see them, and no restart will bring them back: whoever serves
    /// this store should stop.
    pub fn lost_answered_puts(&self) -> bool {
        self.unsynced.is_lost
    }

    /// Writes the puts answered and not yet in the log, as one record,
    /// and returns once they are durable. Nothing held writes nothing.
    pub fn sync_puts(&mut self) -> Result<(), StoreError> {
        if self.unsynced.edits.is_empty() {
            return Ok(());
        }
        self.write_held_keys()?;
        let held = self.take_held();
        if let Err(error) = self.log.append(&Record::Puts(held)) {
            self.unsynced.is_lost = true;
            return Err(error.into());
        }
        Ok(())
    }

    /// The held puts, to be written now; none is held afterwards.
    pub(super) fn take_held(&mut self) -> Vec<Edit> {
        self.unsynced.len = 0;
        std::mem::take(&mut self.unsynced.edits)
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

    /// Applies `puts`, sealed already, and holds them for the log, as
    /// [`Store::hold`] does, if every one is to a plain-async table.
    /// Gives them back if they would have to be written now.
    pub(crate) fn hold_sealed(&mut self, puts: Vec<Edit>) -> Result<Option<Vec<Edit>>, StoreError> {
        self.check_writes_for_itself()?;
        if !self.may_answer_early(&puts) {
            return Ok(Some(puts));
        }
        self.hold(puts)
    }

    /// Whether every key is in a table whose writes may be answered
    /// before they are on disk.
    pub(super) fn may_answer_early(&self, edits: &[Edit]) -> bool {
        edits
            .iter()
            .all(|edit| self.tables.engine_of(&edit.key) == crate::log::Engine::PlainAsync)
    }
}
