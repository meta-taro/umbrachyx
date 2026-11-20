//! A store behind a committer as the source of copies and as a copy
//! itself: what a backup reads of it, what a replica is sent from it,
//! and what it takes in when it follows another.

use std::time::Duration;

use super::Writer;
use crate::keys::{Fingerprint, Key};
use crate::store::StoreError;
use crate::store::backup::{BackupError, Moment, Origin};
use crate::store::follow::{Ask, Chained, Feed, Follower, Held, KeyAnswer, Refusal, Sent};

impl Writer {
    /// One turn of a backup: what it reads of the store, between two
    /// groups of writes.
    fn back_up<T>(
        &self,
        read: impl FnOnce(&mut dyn Origin) -> Result<T, BackupError>,
    ) -> Result<T, BackupError> {
        let mut answer = None;
        let _turn = self.engine.turns().take_after_a_writer();
        self.engine
            .back_up(Box::new(|store| answer = Some(read(store))));
        answer.unwrap_or(Err(BackupError::Stopped))
    }
}

/// A store behind a committer is backed up through any of its writers,
/// while the others write: each turn of the backup waits for the group
/// being written and holds up the next for as long as it reads.
impl Origin for Writer {
    fn records(&mut self) -> Result<u64, BackupError> {
        self.back_up(|store| store.records())
    }

    fn catch_up(&mut self) -> Result<bool, BackupError> {
        self.back_up(|store| store.catch_up())
    }

    fn moment(&mut self, held_records: u64) -> Result<Moment, BackupError> {
        self.back_up(|store| store.moment(held_records))
    }

    fn log_bytes(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, BackupError> {
        self.back_up(|store| store.log_bytes(offset, len))
    }
}

impl Writer {
    /// Whether the store behind this writer follows another, and so
    /// refuses every write.
    pub fn is_following(&self) -> bool {
        self.engine.is_following()
    }

    /// Waits until the log holds more than `after` records, `at_most`
    /// this long, and returns whether it does. For whoever answers a
    /// replica that holds everything: the answer can wait for something
    /// to send, so the replica has it as soon as it is written and need
    /// not ask over and over to find out.
    pub fn wait_for_more_records(&self, after: u64, at_most: Duration) -> bool {
        self.engine.wait_for_records(after, at_most)
    }

    /// How many records a replica has said it holds, and whether writes
    /// are refused because none keeps up. `None` for a store that does
    /// not wait for replicas.
    pub fn replicas(&self) -> Option<(u64, bool)> {
        self.engine.replicas()
    }

    /// How many records the log held when the last write ended.
    pub fn held_records(&self) -> u64 {
        self.engine.records()
    }

    /// One turn of answering a replica, between two groups of writes.
    fn feed<T>(
        &self,
        read: impl FnOnce(&mut dyn Feed) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut answer = None;
        let _turn = self.engine.turns().take_to_feed();
        self.engine
            .feed(Box::new(|store| answer = Some(read(store))));
        answer.unwrap_or(Err(StoreError::Stopped))
    }

    /// One turn of a store that follows another.
    fn follow<T>(
        &self,
        turn: impl FnOnce(&mut dyn Follower) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if !self.engine.is_following() {
            return Err(StoreError::NotFollowing);
        }
        let mut answer = None;
        let _turn = self.engine.turns().take();
        self.engine
            .follow(Box::new(|store| answer = Some(turn(store))));
        answer.unwrap_or(Err(StoreError::Stopped))
    }
}

/// Whichever the store behind it is, one that feeds replicas or one that
/// follows: its chain is worked out a turn at a time, as for a backup.
impl Chained for Writer {
    fn chain_step(&mut self) -> Result<bool, StoreError> {
        self.feed(|store| store.chain_step())
    }
}

/// A store behind a committer feeds replicas through any of its writers,
/// while the others write. A replica is sent what was acknowledged by
/// then; commits queued and not yet written are not waited for.
impl Feed for Writer {
    fn sent(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, StoreError> {
        let sent = self.feed(|store| store.sent(ask))?;
        // The log's name and the chain over what the replica holds
        // checked out: it has those records on its disk.
        if sent.is_ok() {
            self.engine.replica_holds(ask.held.records);
        }
        Ok(sent)
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Vec<KeyAnswer>, StoreError> {
        self.feed(|store| store.keys(wanted))
    }
}

/// A store started with [`Committer::start_following`] takes in its
/// primary's records through any of its writers, and readers see them
/// once it has.
impl Follower for Writer {
    fn held(&mut self) -> Result<Held, StoreError> {
        self.follow(|store| store.held())
    }

    fn take_in(&mut self, sent: &Sent) -> Result<u64, StoreError> {
        self.follow(|store| store.take_in(sent))
    }

    fn lacks(&mut self, sent: &Sent) -> Result<Vec<Fingerprint>, StoreError> {
        self.follow(|store| store.lacks(sent))
    }

    fn take_keys(&mut self, sent: &Sent, keys: &[Key]) -> Result<(), StoreError> {
        self.follow(|store| store.take_keys(sent, keys))
    }
}
