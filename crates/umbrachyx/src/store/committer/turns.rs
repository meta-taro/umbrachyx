//! Whose turn it is to write: the commits queued, the answers left for
//! whoever sent them, and the one turn the threads take in order.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::Answer;
use crate::log::MAX_PAYLOAD_LEN;
use crate::log::record::BATCH_OVERHEAD;
use crate::store::Request;

pub(super) struct Job {
    pub(super) request: Request,
    /// The bytes the request adds to a group's payload.
    pub(super) len: usize,
    pub(super) answer: Arc<Slot>,
}

/// Where a commit's answer is left for whoever sent it.
#[derive(Default)]
pub(super) struct Slot(Mutex<Option<Answer>>);

impl Slot {
    pub(super) fn fill(&self, answer: Answer) {
        *lock(&self.0) = Some(answer);
    }

    pub(super) fn take(&self) -> Option<Answer> {
        lock(&self.0).take()
    }
}

/// A lock a panic elsewhere cannot leave unusable: what it guards is
/// replaced whole, never left half changed.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The commits not yet written, and whether somebody is writing.
#[derive(Default)]
pub(super) struct Turns {
    pub(super) queue: Mutex<Queue>,
    /// Told whenever a turn ends.
    pub(super) ended: Condvar,
}

#[derive(Default)]
pub(super) struct Queue {
    pub(super) jobs: VecDeque<Job>,
    pub(super) is_taken: bool,
    /// Whether whoever has the turn waits for a replica to hold what it
    /// wrote, and lets replicas be answered meanwhile.
    is_lent: bool,
}

/// How long a backup's turn waits for a writer to take its own first.
const LET_A_WRITER_GO: Duration = Duration::from_millis(10);

/// The turn to write, held by one thread at a time. Dropping it, also by a
/// panic, lets the next one take it.
pub(super) struct Turn<'a>(pub(super) &'a Turns);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        lock(&self.0.queue).is_taken = false;
        self.0.ended.notify_all();
    }
}

impl Turns {
    /// Waits for the turn to write.
    pub(super) fn take(&self) -> Turn<'_> {
        let mut queue = lock(&self.queue);
        while queue.is_taken {
            queue = self
                .ended
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
        queue.is_taken = true;
        Turn(self)
    }

    /// Waits for the turn as [`Turns::take`] does, after letting whoever
    /// is writing, or about to, have theirs: for work that takes many
    /// turns in a row and must not keep the writers waiting through all
    /// of them. A commit queued and never waited for has no writer
    /// coming, so the wait for one is short.
    pub(super) fn take_after_a_writer(&self) -> Turn<'_> {
        let queue = lock(&self.queue);
        if queue.is_taken || !queue.jobs.is_empty() {
            drop(
                self.ended
                    .wait_timeout(queue, LET_A_WRITER_GO)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        } else {
            drop(queue);
        }
        self.take()
    }

    /// Waits for the turn as [`Turns::take_after_a_writer`] does, to
    /// answer a replica. `None` if whoever has the turn waits for a
    /// replica: the answer is what it waits for, and goes ahead with the
    /// turn still taken. The store is locked for each read all the same.
    pub(super) fn take_to_feed(&self) -> Option<Turn<'_>> {
        let mut queue = lock(&self.queue);
        if !queue.is_lent && (queue.is_taken || !queue.jobs.is_empty()) {
            queue = self
                .ended
                .wait_timeout(queue, LET_A_WRITER_GO)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        loop {
            if queue.is_lent {
                return None;
            }
            if !queue.is_taken {
                queue.is_taken = true;
                return Some(Turn(self));
            }
            queue = self
                .ended
                .wait(queue)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Lets replicas be answered while whoever has the turn runs `wait`.
    pub(super) fn lend_while<T>(&self, wait: impl FnOnce() -> T) -> T {
        lock(&self.queue).is_lent = true;
        self.ended.notify_all();
        let waited = wait();
        lock(&self.queue).is_lent = false;
        waited
    }

    /// The jobs at the front of the queue whose payload fits one record,
    /// at least one if there is any.
    pub(super) fn next_group(&self) -> Vec<Job> {
        let mut queue = lock(&self.queue);
        // A group of commits alone has less in front of it than a batch;
        // counting a batch's keeps either within one record.
        let mut len = BATCH_OVERHEAD;
        let mut group = Vec::new();
        while let Some(job) = queue.jobs.front() {
            if !group.is_empty() && len + job.len > MAX_PAYLOAD_LEN {
                break;
            }
            len += job.len;
            group.extend(queue.jobs.pop_front());
        }
        group
    }
}
