//! Group commit without a thread of its own: whoever waits for a commit
//! writes it, together with every commit queued by then, with one sync.
//!
//! A sync costs the same for one commit as for many, so writers that would
//! each wait for their own sync share one instead. Commits are queued in the
//! order sent. A writer that waits and finds nobody writing takes the queue
//! and writes it, its own commit and everyone else's; the others sleep until
//! theirs is answered, and the commits that arrive during that sync are the
//! next writer's group. A lone writer therefore writes its own commit on its
//! own thread: handing it to another thread and being woken with the answer
//! cost two wake-ups, more than the store's own work around the sync. Each
//! writer is still answered only once its commit is durable.
//!
//! Readers on other threads read a [`View`] published after each group,
//! before the group's writers are answered, so a write is seen by every
//! reader by the time its writer hears it is durable.
//!
//! Questions about the history, which a view does not hold, and decisions
//! about changes waiting take the same turn as a writer and write the queue
//! first, so each sees every commit sent before it.
//!
//! A commit sent and never waited for is written by the next writer, the
//! next question, or the stop.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};

use super::{Clock, CommitId, CommitRequest, Inspect, Provenance, Review, Store, StoreError, View};
use crate::log::record::{GROUP_OVERHEAD, commit_len};
use crate::log::{Edit, LogError, MAX_PAYLOAD_LEN, Storage};

type Answer = Result<CommitId, StoreError>;

/// Reads the store.
type Question<'a> = Box<dyn FnOnce(&dyn Inspect) + 'a>;
/// Approves or rejects a change.
type Decision<'a> = Box<dyn FnOnce(&mut dyn Review) + 'a>;

struct Job {
    request: CommitRequest,
    /// The bytes the request adds to a group's payload.
    len: usize,
    answer: Arc<Slot>,
}

/// Where a commit's answer is left for whoever sent it.
#[derive(Default)]
struct Slot(Mutex<Option<Answer>>);

impl Slot {
    fn fill(&self, answer: Answer) {
        *lock(&self.0) = Some(answer);
    }

    fn take(&self) -> Option<Answer> {
        lock(&self.0).take()
    }
}

/// A lock a panic elsewhere cannot leave unusable: what it guards is
/// replaced whole, never left half changed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The commits not yet written, and whether somebody is writing.
#[derive(Default)]
struct Turns {
    queue: Mutex<Queue>,
    /// Told whenever a turn ends.
    ended: Condvar,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    is_taken: bool,
}

/// The turn to write, held by one thread at a time. Dropping it, also by a
/// panic, lets the next one take it.
struct Turn<'a>(&'a Turns);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        lock(&self.0.queue).is_taken = false;
        self.0.ended.notify_all();
    }
}

impl Turns {
    /// Waits for the turn to write.
    fn take(&self) -> Turn<'_> {
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

    /// The jobs at the front of the queue whose payload fits one record,
    /// at least one if there is any.
    fn next_group(&self) -> Vec<Job> {
        let mut queue = lock(&self.queue);
        let mut len = GROUP_OVERHEAD;
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

/// The store behind the turns, with its storage and clock hidden so that
/// writers need not name them.
trait Engine: Send + Sync {
    fn turns(&self) -> &Turns;
    /// Writes a group and answers it. The caller holds the turn.
    fn write(&self, group: Vec<Job>);
    /// Whether the store was there to ask. The caller holds the turn.
    fn inspect(&self, question: Question<'_>) -> bool;
    /// As `inspect`, and publishes the view after it.
    fn review(&self, decision: Decision<'_>) -> bool;
}

struct Core<S: Storage, C: Clock> {
    turns: Turns,
    /// `None` once stopped.
    store: Mutex<Option<Store<S, C>>>,
    published: Published,
}

impl<S: Storage, C: Clock> Core<S, C> {
    fn write(&self, group: Vec<Job>) {
        match lock(&self.store).as_mut() {
            Some(store) => write_group(store, group, &self.published),
            None => {
                for job in group {
                    job.answer.fill(Err(StoreError::Stopped));
                }
            }
        }
    }

    /// Writes everything queued. The caller holds the turn.
    fn write_queue(&self) {
        loop {
            let group = self.turns.next_group();
            if group.is_empty() {
                return;
            }
            self.write(group);
        }
    }

    fn publish(&self, store: &Store<S, C>) {
        let view = Arc::new(store.view());
        *self
            .published
            .write()
            .unwrap_or_else(PoisonError::into_inner) = view;
    }

    /// Writes what is queued and takes the store out.
    fn stop(&self) -> Option<Store<S, C>> {
        let _turn = self.turns.take();
        self.write_queue();
        lock(&self.store).take()
    }
}

impl<S, C> Engine for Core<S, C>
where
    S: Storage + Send + 'static,
    C: Clock + Send + 'static,
{
    fn turns(&self) -> &Turns {
        &self.turns
    }

    fn write(&self, group: Vec<Job>) {
        Core::write(self, group);
    }

    fn inspect(&self, question: Question<'_>) -> bool {
        self.write_queue();
        match lock(&self.store).as_ref() {
            Some(store) => {
                question(store);
                true
            }
            None => false,
        }
    }

    fn review(&self, decision: Decision<'_>) -> bool {
        self.write_queue();
        match lock(&self.store).as_mut() {
            Some(store) => {
                decision(store);
                // An approval moves main; readers see it from here on.
                self.publish(store);
                true
            }
            None => false,
        }
    }
}

/// Holds a store that many threads write to, grouping their commits.
pub struct Committer<S: Storage, C: Clock> {
    core: Arc<Core<S, C>>,
}

/// The latest view. Replacing it swaps one pointer, so readers wait on the
/// lock no longer than that.
type Published = Arc<RwLock<Arc<View>>>;

/// Sends commits to a [`Committer`]. Cheap to clone, one per writer thread.
#[derive(Clone)]
pub struct Writer {
    engine: Arc<dyn Engine>,
}

/// Reads the views a [`Committer`] publishes. Cheap to clone, and still
/// reads the last view once the committer has stopped.
#[derive(Clone)]
pub struct Reader {
    published: Published,
}

impl<S, C> Committer<S, C>
where
    S: Storage + Send + 'static,
    C: Clock + Send + 'static,
{
    pub fn start(store: Store<S, C>) -> Self {
        let published = Arc::new(RwLock::new(Arc::new(store.view())));
        Self {
            core: Arc::new(Core {
                turns: Turns::default(),
                store: Mutex::new(Some(store)),
                published,
            }),
        }
    }

    pub fn writer(&self) -> Writer {
        Writer {
            engine: Arc::clone(&self.core) as Arc<dyn Engine>,
        }
    }

    pub fn reader(&self) -> Reader {
        Reader {
            published: Arc::clone(&self.core.published),
        }
    }

    /// Writes and answers every commit sent before this call, then gives
    /// the store back. Commits sent afterwards are refused with `Stopped`.
    pub fn stop(self) -> Store<S, C> {
        self.core.stop().expect("only stop and drop take the store")
    }
}

impl<S: Storage, C: Clock> Drop for Committer<S, C> {
    fn drop(&mut self) {
        // Commits already sent are written, as they were when a thread
        // wrote them; `stop` has left nothing to do.
        drop(self.core.stop());
    }
}

impl Writer {
    /// Writes `edits` to `branch` as one commit and returns its id once it
    /// is durable, as [`Store::commit`] does.
    pub fn commit(
        &self,
        branch: &str,
        edits: Vec<Edit>,
        provenance: Provenance,
    ) -> Result<CommitId, StoreError> {
        self.send(branch, edits, provenance).wait()
    }

    /// Queues a commit without waiting for it, so commits sent one after
    /// another can share a sync. They are written in the order sent, once
    /// one of them, or any other, is waited for.
    pub fn send(&self, branch: &str, edits: Vec<Edit>, provenance: Provenance) -> Pending {
        let len = commit_len(
            branch,
            &provenance.who,
            &provenance.why,
            provenance.approved_by.as_deref(),
            false,
            &edits,
        );
        let request = CommitRequest {
            branch: branch.to_owned(),
            edits,
            provenance,
        };
        let answer = Arc::new(Slot::default());
        lock(&self.engine.turns().queue).jobs.push_back(Job {
            request,
            len,
            answer: Arc::clone(&answer),
        });
        Pending {
            answer,
            engine: Arc::clone(&self.engine),
        }
    }

    /// Answers `question` from the store once the commits sent before it
    /// are written. It waits behind their sync, so it suits a person
    /// reading the history, not a hot path; reads of live values go
    /// through a [`Reader`].
    pub fn ask<T>(&self, question: impl FnOnce(&dyn Inspect) -> T) -> Result<T, StoreError> {
        let mut answer = None;
        let _turn = self.engine.turns().take();
        self.engine
            .inspect(Box::new(|store| answer = Some(question(store))));
        answer.ok_or(StoreError::Stopped)
    }

    /// Carries out `decision` on the store once the commits sent before it
    /// are written, and publishes the view after it.
    pub fn decide<T>(&self, decision: impl FnOnce(&mut dyn Review) -> T) -> Result<T, StoreError> {
        let mut answer = None;
        let _turn = self.engine.turns().take();
        self.engine
            .review(Box::new(|store| answer = Some(decision(store))));
        answer.ok_or(StoreError::Stopped)
    }
}

/// A commit sent to a [`Committer`] and not yet answered.
pub struct Pending {
    answer: Arc<Slot>,
    engine: Arc<dyn Engine>,
}

impl Pending {
    /// The commit's id once it is durable. If nobody is writing, this
    /// thread writes: its own commit and every other one queued.
    pub fn wait(self) -> Result<CommitId, StoreError> {
        let turns = self.engine.turns();
        let mut queue = lock(&turns.queue);
        loop {
            // Checked under the queue's lock, which a turn takes to end:
            // an answer left while this thread was not looking is seen
            // here, and one left after it wakes the wait below.
            if let Some(answer) = self.answer.take() {
                return answer;
            }
            if queue.is_taken {
                queue = turns
                    .ended
                    .wait(queue)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            queue.is_taken = true;
            drop(queue);
            {
                let _turn = Turn(turns);
                let group = turns.next_group();
                self.engine.write(group);
            }
            queue = lock(&turns.queue);
        }
    }
}

impl Reader {
    /// Every branch as of the latest group written.
    pub fn view(&self) -> Arc<View> {
        // Only a pointer is ever written under the lock, so a writer that
        // panicked holding it cannot have left it half changed.
        Arc::clone(
            &self
                .published
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

fn write_group<S: Storage, C: Clock>(
    store: &mut Store<S, C>,
    group: Vec<Job>,
    published: &Published,
) {
    let (requests, slots): (Vec<_>, Vec<_>) = group
        .into_iter()
        .map(|job| (job.request, job.answer))
        .unzip();
    match store.commit_all(requests) {
        Ok(answers) => {
            // Published before any answer is left: a writer that finds its
            // answer must be able to read its write.
            let view = Arc::new(store.view());
            *published.write().unwrap_or_else(PoisonError::into_inner) = view;
            for (slot, answer) in slots.into_iter().zip(answers) {
                slot.fill(answer);
            }
        }
        Err(error) => {
            for slot in slots {
                slot.fill(Err(copy(&error)));
            }
        }
    }
}

/// The same error for every writer in a failed group. An I/O error cannot
/// be cloned, so each gets one of the same kind and message.
fn copy(error: &StoreError) -> StoreError {
    match error {
        StoreError::Log(LogError::Io(io)) => {
            StoreError::Log(LogError::Io(io::Error::new(io.kind(), io.to_string())))
        }
        StoreError::Log(LogError::RecordTooLarge { len }) => {
            StoreError::Log(LogError::RecordTooLarge { len: *len })
        }
        StoreError::Log(LogError::Poisoned) => StoreError::Log(LogError::Poisoned),
        other => StoreError::Inconsistent(other.to_string()),
    }
}
