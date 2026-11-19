//! What a store that answers a write only once a replica holds it keeps
//! track of: how many records a replica has said it holds, and whether
//! any replica is keeping up.
//!
//! A replica's ask says how many records it has on its disk, and the
//! primary has checked the log's name and the chain over them before it
//! counts the ask. After a restart nothing is counted yet: a replica
//! says again what it holds with its next ask.
//!
//! A write waits for that count to reach it, for a limit. When the limit
//! passes, the write is in this store's log and no replica has said it
//! holds it; its writer is told exactly that. From then on writes are
//! refused before they are written, so a server whose replica is gone
//! does not go on taking writes it would answer the same way, until a
//! replica holds everything the log does.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

#[derive(Debug, Default)]
struct State {
    /// The most records any replica has said it holds.
    held: u64,
    /// Whether a write gave up waiting, and no replica has caught up
    /// since.
    is_behind: bool,
}

#[derive(Debug)]
pub(super) struct Replicas {
    state: Mutex<State>,
    moved: Condvar,
    /// The longest a write waits.
    limit: Duration,
}

impl Replicas {
    pub(super) fn new(limit: Duration) -> Self {
        Self {
            state: Mutex::default(),
            moved: Condvar::new(),
            limit,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A replica holds the first `held` records of the log, which holds
    /// `records` now.
    pub(super) fn holds(&self, held: u64, records: u64) {
        let mut state = self.state();
        state.held = state.held.max(held);
        if state.held >= records {
            state.is_behind = false;
        }
        self.moved.notify_all();
    }

    /// Whether writes are refused before they are written.
    pub(super) fn is_behind(&self) -> bool {
        self.state().is_behind
    }

    /// The most records a replica has said it holds.
    pub(super) fn held(&self) -> u64 {
        self.state().held
    }

    /// Waits until a replica holds `records` records, the limit at most,
    /// and returns whether one does.
    pub(super) fn wait_for(&self, records: u64) -> bool {
        let state = self.state();
        let (mut state, _) = self
            .moved
            .wait_timeout_while(state, self.limit, |state| state.held < records)
            .unwrap_or_else(PoisonError::into_inner);
        if state.held < records {
            state.is_behind = true;
        }
        state.held >= records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_a_replica_holds_is_not_waited_for() {
        let replicas = Replicas::new(Duration::from_secs(30));
        replicas.holds(5, 5);

        assert!(replicas.wait_for(5));
        assert!(!replicas.is_behind());
    }

    #[test]
    fn a_write_no_replica_holds_in_time_closes_the_store_until_one_holds_everything() {
        let replicas = Replicas::new(Duration::from_millis(10));

        assert!(!replicas.wait_for(3));
        assert!(replicas.is_behind());
        // Some of it is not all of it.
        replicas.holds(2, 3);
        assert!(replicas.is_behind());
        replicas.holds(3, 3);
        assert!(!replicas.is_behind());
        // What an older ask said does not take it back.
        replicas.holds(1, 3);
        assert_eq!(replicas.held(), 3);
    }
}
