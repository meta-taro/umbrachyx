//! Whose turn it is to write: the commits queued, the answers left for
//! whoever sent them, and the one turn the threads take in order.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::Answer;
use crate::log::MAX_PAYLOAD_LEN;
use crate::log::record::BATCH_OVERHEAD;
use crate::store::Request;

pub(super) struct Job {
    pub(super) request: Request,
    /// The bytes the request adds to a group's payload.
    pub(super) len: usize,
    pub(super) answer: Arc<Slot>,
    /// Whether writing it waits for the disk: anything but puts that
    /// are answered before they are on it.
    pub(super) waits_for_the_disk: bool,
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
    ended: Condvar,
}

#[derive(Default)]
pub(super) struct Queue {
    pub(super) jobs: VecDeque<Job>,
    pub(super) is_taken: bool,
    /// Whether whoever has the turn may keep it for as long as the disk
    /// takes, or longer: every turn but one taken to hold puts, or to
    /// write a queue that has only such puts in it so far.
    is_long: bool,
    /// How many of the jobs queued wait for the disk.
    long_jobs: usize,
    /// How many threads wait to be told that a turn ended.
    waiting: usize,
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
        let mut queue = lock(&self.0.queue);
        queue.is_taken = false;
        queue.is_long = false;
        // Telling costs a system call on some systems even when nobody
        // listens, and a thread that answers puts itself ends a turn
        // for every connection it reads.
        let is_awaited = queue.waiting > 0;
        drop(queue);
        if is_awaited {
            self.0.ended.notify_all();
        }
    }
}

impl Turns {
    /// Waits to be told that a turn ended. Every wait for that goes
    /// through here or [`Turns::wait_at_most`]: a turn that ends tells
    /// only the threads counted.
    pub(super) fn wait<'a>(&self, mut queue: MutexGuard<'a, Queue>) -> MutexGuard<'a, Queue> {
        queue.waiting += 1;
        let mut queue = self
            .ended
            .wait(queue)
            .unwrap_or_else(PoisonError::into_inner);
        queue.waiting -= 1;
        queue
    }

    fn wait_at_most<'a>(
        &self,
        mut queue: MutexGuard<'a, Queue>,
        at_most: Duration,
    ) -> MutexGuard<'a, Queue> {
        queue.waiting += 1;
        let (mut queue, _) = self
            .ended
            .wait_timeout(queue, at_most)
            .unwrap_or_else(PoisonError::into_inner);
        queue.waiting -= 1;
        queue
    }

    /// Queues `job` for whoever writes next.
    pub(super) fn queue(&self, job: Job) {
        let mut queue = lock(&self.queue);
        queue.long_jobs += usize::from(job.waits_for_the_disk);
        queue.jobs.push_back(job);
    }

    /// The turn to write, if within `at_most` nobody has it and nothing
    /// is queued: for a thread that must not sleep, and whose write must
    /// not go ahead of one queued before it. It looks again and again
    /// rather than sleep, so `at_most` is a few turns' length, no more;
    /// and it does not look at all while the turn is one that may last
    /// as long as the disk takes, or a write that waits for the disk is
    /// queued: looking through a sync kept every connection of the
    /// thread waiting, its reads too.
    pub(super) fn take_if_idle(&self, at_most: Duration) -> Option<Turn<'_>> {
        let since = Instant::now();
        loop {
            let mut queue = lock(&self.queue);
            if queue.long_jobs > 0 || (queue.is_taken && queue.is_long) {
                return None;
            }
            if !queue.is_taken && queue.jobs.is_empty() {
                queue.is_taken = true;
                return Some(Turn(self));
            }
            drop(queue);
            if since.elapsed() >= at_most {
                return None;
            }
            std::hint::spin_loop();
        }
    }

    /// Waits for the turn to write.
    pub(super) fn take(&self) -> Turn<'_> {
        let mut queue = self.wait_until_free();
        queue.is_taken = true;
        queue.is_long = true;
        Turn(self)
    }

    /// Waits for the turn as [`Turns::take`] does, to write the queue:
    /// the turn is long only once [`Turns::next_group`] hands it a job
    /// that waits for the disk.
    pub(super) fn take_to_write_the_queue(&self) -> Turn<'_> {
        let mut queue = self.wait_until_free();
        queue.is_taken = true;
        Turn(self)
    }

    fn wait_until_free(&self) -> MutexGuard<'_, Queue> {
        let mut queue = lock(&self.queue);
        while queue.is_taken {
            queue = self.wait(queue);
        }
        queue
    }

    /// Takes the turn for a thread that found it free under the queue's
    /// lock, to write a group.
    pub(super) fn take_found_free<'a>(&'a self, mut queue: MutexGuard<'_, Queue>) -> Turn<'a> {
        queue.is_taken = true;
        queue.is_long = true;
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
            drop(self.wait_at_most(queue, LET_A_WRITER_GO));
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
            queue = self.wait_at_most(queue, LET_A_WRITER_GO);
        }
        drop(queue);
        self.take_to_read()
    }

    /// Waits for the turn as [`Turns::take`] does, to read the store.
    /// `None` if whoever has the turn waits for a replica: what it wrote
    /// is in the store, and reading does not wait with it.
    pub(super) fn take_to_read(&self) -> Option<Turn<'_>> {
        let mut queue = lock(&self.queue);
        loop {
            if queue.is_lent {
                return None;
            }
            if !queue.is_taken {
                queue.is_taken = true;
                queue.is_long = true;
                return Some(Turn(self));
            }
            queue = self.wait(queue);
        }
    }

    /// Lets replicas be answered, and the store be read, while whoever
    /// has the turn runs `wait`.
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
            if job.waits_for_the_disk {
                queue.long_jobs -= 1;
                queue.is_long = true;
            }
            group.extend(queue.jobs.pop_front());
        }
        group
    }
}
