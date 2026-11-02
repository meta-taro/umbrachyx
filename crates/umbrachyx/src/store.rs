//! The store: the log is the truth, and the tree is rebuilt from it.
//!
//! A commit is acknowledged once its record is synced to the log; only then
//! is it applied to the tree. Opening the store replays the log into a fresh
//! tree, so nothing the tree holds can be missing from the log.
//!
//! Only `main` exists so far. Branches come with commits as objects.

use std::sync::Arc;
use std::time::SystemTime;

use crate::log::{Edit, Log, LogError, Record, Storage};
use crate::tree::{Bytes, Tree};

/// The branch that holds live data.
pub const MAIN_BRANCH: &str = "main";

/// Where the store reads the time. Tests replace it with a clock a seed
/// controls, the same way they replace the disk.
pub trait Clock {
    /// Microseconds since the Unix epoch. May go backwards; the store never
    /// records a time earlier than one it has already recorded.
    fn now_micros(&mut self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_micros(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
            })
    }
}

/// Who made a commit, why, and who approved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub who: String,
    pub why: String,
    pub approved_by: Option<String>,
}

pub struct Store<S: Storage, C: Clock> {
    log: Log<S>,
    clock: C,
    main: Tree,
    /// The time of the latest commit, so times never go backwards.
    last_time: u64,
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Opens the log and replays every commit into the tree.
    pub fn open(storage: S, clock: C) -> Result<Self, LogError> {
        let (log, records) = Log::open(storage)?;
        let mut store = Self {
            log,
            clock,
            main: Tree::empty(),
            last_time: 0,
        };
        for record in &records {
            store.apply(record);
        }
        Ok(store)
    }

    /// Writes `edits` to `main` as one commit. Returns once the commit is
    /// durable. If it fails, the tree is unchanged and, as with the log, the
    /// store refuses further commits.
    pub fn commit(&mut self, edits: Vec<Edit>, provenance: Provenance) -> Result<(), LogError> {
        let record = Record {
            branch: MAIN_BRANCH.to_owned(),
            changes: edits,
            who: provenance.who,
            why: provenance.why,
            approved_by: provenance.approved_by,
            time: self.clock.now_micros().max(self.last_time),
        };
        self.log.append(&record)?;
        self.apply(&record);
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Option<&Bytes> {
        self.main.get(key)
    }

    /// The current state of `main`. Cloning it is one pointer copy, and the
    /// clone does not change when later commits are made.
    pub fn main(&self) -> &Tree {
        &self.main
    }

    /// The time recorded on the latest commit.
    pub fn last_time(&self) -> u64 {
        self.last_time
    }

    fn apply(&mut self, record: &Record) {
        for edit in &record.changes {
            self.main = match &edit.value {
                Some(value) => self
                    .main
                    .insert(Arc::from(edit.key.as_slice()), Arc::from(value.as_slice())),
                None => self.main.remove(&edit.key),
            };
        }
        self.last_time = self.last_time.max(record.time);
    }

    #[cfg(test)]
    pub(crate) fn storage_mut(&mut self) -> &mut S {
        self.log.storage_mut()
    }

    #[cfg(test)]
    pub(crate) fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }
}
