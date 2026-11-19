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
//!
//! Puts to plain-async tables are the one exception to "answered once
//! durable": a group made only of them is applied and answered without a
//! sync, and the store holds them until the next record it writes, a call
//! to [`Writer::sync_puts`], or the stop. Nothing here keeps time: whoever
//! runs the committer calls `sync_puts` as often as it promises to.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use super::backup::{BackupError, Moment, Origin};
use super::follow::{Ask, Chained, Feed, Follower, Held, Refusal, Sent};
use super::replicas::Replicas;
use super::{
    Clock, CommitId, CommitRequest, Inspect, Provenance, Request, Review, Store, StoreError, View,
};
use crate::log::record::{BATCH_OVERHEAD, commit_len, edits_len};
use crate::log::{Edit, LogError, MAX_PAYLOAD_LEN, Storage};

type Answer = Result<CommitId, StoreError>;

/// Reads the store.
type Question<'a> = Box<dyn FnOnce(&dyn Inspect) + 'a>;
/// Approves or rejects a change.
type Decision<'a> = Box<dyn FnOnce(&mut dyn Review) + 'a>;
/// Reads what a backup needs of the store.
type BackupTurn<'a> = Box<dyn FnOnce(&mut dyn Origin) + 'a>;
/// Reads what a replica is sent.
type FeedTurn<'a> = Box<dyn FnOnce(&mut dyn Feed) + 'a>;
/// Takes in what a primary sent.
type FollowTurn<'a> = Box<dyn FnOnce(&mut dyn Follower) + 'a>;

struct Job {
    request: Request,
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
    /// Whether whoever has the turn waits for a replica to hold what it
    /// wrote, and lets replicas be answered meanwhile.
    is_lent: bool,
}

/// How long a backup's turn waits for a writer to take its own first.
const LET_A_WRITER_GO: Duration = Duration::from_millis(10);

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

    /// Waits for the turn as [`Turns::take`] does, after letting whoever
    /// is writing, or about to, have theirs: for work that takes many
    /// turns in a row and must not keep the writers waiting through all
    /// of them. A commit queued and never waited for has no writer
    /// coming, so the wait for one is short.
    fn take_after_a_writer(&self) -> Turn<'_> {
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
    fn take_to_feed(&self) -> Option<Turn<'_>> {
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
    fn lend_while<T>(&self, wait: impl FnOnce() -> T) -> T {
        lock(&self.queue).is_lent = true;
        self.ended.notify_all();
        let waited = wait();
        lock(&self.queue).is_lent = false;
        waited
    }

    /// The jobs at the front of the queue whose payload fits one record,
    /// at least one if there is any.
    fn next_group(&self) -> Vec<Job> {
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

/// The store behind the turns, with its storage and clock hidden so that
/// writers need not name them.
trait Engine: Send + Sync {
    fn turns(&self) -> &Turns;
    /// Writes a group and answers it. The caller holds the turn.
    fn write(&self, group: Vec<Job>);
    /// Writes everything queued. The caller holds the turn.
    fn write_queue(&self);
    /// Whether the store was there to ask. The caller holds the turn.
    fn inspect(&self, question: Question<'_>) -> bool;
    /// As `inspect`, and publishes the view after it. Refused, the
    /// decision not carried out, if the store was not there or no
    /// replica keeps up; an error after it was carried out if no replica
    /// held it in time.
    fn review(&self, decision: Decision<'_>) -> Result<(), StoreError>;
    /// A replica said it holds this many records, and they checked out.
    fn replica_holds(&self, held: u64);
    /// How many records a replica has said it holds and whether writes
    /// are refused for want of one, if the store waits for replicas.
    fn replicas(&self) -> Option<(u64, bool)>;
    /// As `inspect`, for a backup, without writing the queue first: a
    /// backup holds what was acknowledged, whatever waits. The caller
    /// holds the turn.
    fn back_up(&self, turn: BackupTurn<'_>) -> bool;
    /// As `back_up`, for a replica that asks for records: it is sent
    /// only what was acknowledged. The caller holds the turn.
    fn feed(&self, turn: FeedTurn<'_>) -> bool;
    /// Whether the store follows another, and so takes no writes.
    fn is_following(&self) -> bool;
    /// As `review`, for a store that follows another. The caller holds
    /// the turn.
    fn follow(&self, turn: FollowTurn<'_>) -> bool;
    /// Waits until the log holds more than `after` records, `at_most`
    /// this long, and returns whether it does. Needs no turn.
    fn wait_for_records(&self, after: u64, at_most: Duration) -> bool;
    /// How many records the log held when the last write ended.
    fn records(&self) -> u64;
    /// Whether puts were answered that the log does not hold yet, as of
    /// the last group written.
    fn has_unsynced_puts(&self) -> bool;
    /// Writes the queue, then the puts the log does not hold yet. The
    /// caller holds the turn.
    fn sync_puts(&self) -> Result<(), StoreError>;
    /// Whether the log refused puts that were already answered.
    fn lost_answered_puts(&self) -> bool;
}

struct Core<S: Storage, C: Clock> {
    turns: Turns,
    /// `None` once stopped.
    store: Mutex<Option<Store<S, C>>>,
    published: Published,
    /// Whether the store follows another: its records come from there,
    /// and every write sent here is refused.
    is_following: bool,
    /// Whether the store holds puts the log does not, kept here so that
    /// asking does not wait for the store.
    has_unsynced_puts: AtomicBool,
    /// Whether the log refused puts that were already answered.
    lost_answered_puts: AtomicBool,
    /// How many records the log holds, kept here so that whoever waits
    /// for more does not hold the store while waiting.
    records: Mutex<u64>,
    /// Told whenever `records` grows.
    grew: Condvar,
    /// What replicas hold, if a write is answered only once one holds
    /// it.
    replicas: Option<Replicas>,
}

impl<S: Storage, C: Clock> Core<S, C> {
    fn write(&self, group: Vec<Job>) {
        if self.replicas.as_ref().is_some_and(Replicas::is_behind) {
            for job in group {
                job.answer.fill(Err(StoreError::NoReplica));
            }
            return;
        }
        let mut store = lock(&self.store);
        let Some(held) = store.as_mut() else {
            for job in group {
                job.answer.fill(Err(StoreError::Stopped));
            }
            return;
        };
        let before = held.log_records();
        let answers = write_group(held, group, &self.published);
        self.note_unsynced(held);
        let after = held.log_records();
        // Not held while waiting: answering a replica reads the store.
        drop(store);
        let is_held = after == before || self.wait_for_a_replica(after);
        for (slot, answer) in answers {
            slot.fill(match answer {
                Ok(_) if !is_held => Err(StoreError::NotReplicated),
                answer => answer,
            });
        }
    }

    /// Waits until a replica holds the first `records` records, if the
    /// store waits for replicas, and returns whether one does. The
    /// caller holds the turn and not the store.
    fn wait_for_a_replica(&self, records: u64) -> bool {
        match &self.replicas {
            Some(replicas) => self.turns.lend_while(|| replicas.wait_for(records)),
            None => true,
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

    /// Tells whoever waits for more records how many there are now.
    fn note_records(&self, store: &Store<S, C>) {
        let now = store.log_records();
        let mut records = lock(&self.records);
        if *records != now {
            *records = now;
            self.grew.notify_all();
        }
    }

    fn wait_for_records(&self, after: u64, at_most: Duration) -> bool {
        let records = lock(&self.records);
        let (records, _) = self
            .grew
            .wait_timeout_while(records, at_most, |records| *records <= after)
            .unwrap_or_else(PoisonError::into_inner);
        *records > after
    }

    fn note_unsynced(&self, store: &Store<S, C>) {
        self.note_records(store);
        self.has_unsynced_puts
            .store(store.has_unsynced_puts(), Ordering::Release);
        self.lost_answered_puts
            .store(store.lost_answered_puts(), Ordering::Release);
    }

    fn sync_puts(&self) -> Result<(), StoreError> {
        self.write_queue();
        let mut store = lock(&self.store);
        let store = store.as_mut().ok_or(StoreError::Stopped)?;
        let synced = store.sync_puts();
        self.note_unsynced(store);
        synced
    }

    /// Writes what is queued, and the puts answered before they were on
    /// disk, and takes the store out.
    fn stop(&self) -> Option<Store<S, C>> {
        let _turn = self.turns.take();
        // If the log refuses them, it refuses every write from then on:
        // whoever takes the store finds that out from the store itself.
        let _ = self.sync_puts();
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

    fn write_queue(&self) {
        Core::write_queue(self);
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

    fn review(&self, decision: Decision<'_>) -> Result<(), StoreError> {
        self.write_queue();
        if self.replicas.as_ref().is_some_and(Replicas::is_behind) {
            return Err(StoreError::NoReplica);
        }
        let mut store = lock(&self.store);
        let held = store.as_mut().ok_or(StoreError::Stopped)?;
        let before = held.log_records();
        decision(held);
        // An approval moves main; readers see it from here on.
        self.publish(held);
        self.note_unsynced(held);
        let after = held.log_records();
        drop(store);
        if after == before || self.wait_for_a_replica(after) {
            Ok(())
        } else {
            Err(StoreError::NotReplicated)
        }
    }

    fn replica_holds(&self, held: u64) {
        if let Some(replicas) = &self.replicas {
            replicas.holds(held, *lock(&self.records));
        }
    }

    fn replicas(&self) -> Option<(u64, bool)> {
        self.replicas
            .as_ref()
            .map(|replicas| (replicas.held(), replicas.is_behind()))
    }

    fn back_up(&self, turn: BackupTurn<'_>) -> bool {
        match lock(&self.store).as_mut() {
            Some(store) => {
                turn(store);
                true
            }
            None => false,
        }
    }

    fn feed(&self, turn: FeedTurn<'_>) -> bool {
        match lock(&self.store).as_mut() {
            Some(store) => {
                turn(store);
                true
            }
            None => false,
        }
    }

    fn is_following(&self) -> bool {
        self.is_following
    }

    fn follow(&self, turn: FollowTurn<'_>) -> bool {
        match lock(&self.store).as_mut() {
            Some(store) => {
                let before = store.log_records();
                turn(store);
                // Readers see the records taken in from here on.
                if store.log_records() != before {
                    self.publish(store);
                    self.note_records(store);
                }
                true
            }
            None => false,
        }
    }

    fn wait_for_records(&self, after: u64, at_most: Duration) -> bool {
        Core::wait_for_records(self, after, at_most)
    }

    fn records(&self) -> u64 {
        *lock(&self.records)
    }

    fn has_unsynced_puts(&self) -> bool {
        self.has_unsynced_puts.load(Ordering::Acquire)
    }

    fn sync_puts(&self) -> Result<(), StoreError> {
        Core::sync_puts(self)
    }

    fn lost_answered_puts(&self) -> bool {
        self.lost_answered_puts.load(Ordering::Acquire)
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
        Self::with(store, false, None)
    }

    /// Holds a store that answers a commit, a put to a table that is
    /// synced, or a decision only once a replica has said it holds it,
    /// and waits `limit` for that. A write nobody held in time is
    /// answered [`StoreError::NotReplicated`]: it is in this store's log
    /// all the same. Writes after it are refused unwritten
    /// ([`StoreError::NoReplica`]) until a replica holds everything.
    /// Puts answered before they are on disk do not wait.
    pub fn start_waiting_for_a_replica(store: Store<S, C>, limit: Duration) -> Self {
        Self::with(store, false, Some(Replicas::new(limit)))
    }

    /// Holds a store that follows another: its records are taken in
    /// through a writer ([`Follower`]), and commits, puts and decisions
    /// sent to it are refused, whoever sends them. A store that also
    /// wrote for itself would hold records its primary does not, and
    /// could be sent nothing more.
    pub fn start_following(store: Store<S, C>) -> Self {
        Self::with(store, true, None)
    }

    fn with(store: Store<S, C>, is_following: bool, replicas: Option<Replicas>) -> Self {
        let published = Arc::new(RwLock::new(Arc::new(store.view())));
        let records = store.log_records();
        Self {
            core: Arc::new(Core {
                turns: Turns::default(),
                store: Mutex::new(Some(store)),
                published,
                is_following,
                has_unsynced_puts: AtomicBool::new(false),
                lost_answered_puts: AtomicBool::new(false),
                records: Mutex::new(records),
                grew: Condvar::new(),
                replicas,
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
        let request = Request::Commit(CommitRequest {
            branch: branch.to_owned(),
            edits,
            provenance,
        });
        self.queue(request, len)
    }

    /// Queues writes to tables that keep no history, as [`Store::put`]
    /// makes them, without waiting. They are written in the order sent
    /// among everything else queued, with the same sync.
    pub fn put(&self, edits: Vec<Edit>) -> PendingPut {
        let len = edits_len(&edits);
        PendingPut(self.queue(Request::Put(edits), len))
    }

    fn queue(&self, request: Request, len: usize) -> Pending {
        let answer = Arc::new(Slot::default());
        if self.engine.is_following() {
            answer.fill(Err(StoreError::ReadOnly));
        } else {
            lock(&self.engine.turns().queue).jobs.push_back(Job {
                request,
                len,
                answer: Arc::clone(&answer),
            });
        }
        Pending {
            answer,
            engine: Arc::clone(&self.engine),
        }
    }

    /// Writes every commit queued so far and answers them, on this thread.
    /// For a thread kept for that, so that the threads that sent the
    /// commits need not wait for the sync: they ask each [`Pending`] with
    /// [`Pending::poll`] afterwards.
    pub fn write_queued(&self) {
        let _turn = self.engine.turns().take();
        self.engine.write_queue();
    }

    /// Whether puts to plain-async tables were answered that are not on
    /// disk yet, as of the last group written.
    pub fn has_unsynced_puts(&self) -> bool {
        self.engine.has_unsynced_puts()
    }

    /// Whether the log refused puts that were already answered: by a
    /// call to [`Writer::sync_puts`], or by any write that carried them.
    /// Readers see those values and no restart will bring them back, so
    /// whoever serves should say so and stop.
    pub fn lost_answered_puts(&self) -> bool {
        self.engine.lost_answered_puts()
    }

    /// Writes everything queued, then the puts answered before they were
    /// on disk, and returns once they are durable. An error means memory
    /// is ahead of the disk for good: the log takes no more writes, and
    /// what was answered and not written is lost when the process ends.
    pub fn sync_puts(&self) -> Result<(), StoreError> {
        let _turn = self.engine.turns().take();
        self.engine.sync_puts()
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
        if self.engine.is_following() {
            return Err(StoreError::ReadOnly);
        }
        let mut answer = None;
        let _turn = self.engine.turns().take();
        self.engine
            .review(Box::new(|store| answer = Some(decision(store))))?;
        answer.ok_or(StoreError::Stopped)
    }
}

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
}

/// A commit sent to a [`Committer`] and not yet answered.
pub struct Pending {
    answer: Arc<Slot>,
    engine: Arc<dyn Engine>,
}

impl Pending {
    /// The answer, if the commit has been written, without waiting or
    /// writing. The answer is given once.
    pub fn poll(&mut self) -> Option<Result<CommitId, StoreError>> {
        self.answer.take()
    }

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

/// Writes to tables without history sent to a [`Committer`] and not yet
/// answered. There is no commit to name, so the answer is only whether
/// they were written.
pub struct PendingPut(Pending);

impl PendingPut {
    /// The answer, if the puts have been written, without waiting or
    /// writing. The answer is given once.
    pub fn poll(&mut self) -> Option<Result<(), StoreError>> {
        self.0.poll().map(|answer| answer.map(drop))
    }

    /// Returns once the puts are durable. If nobody is writing, this
    /// thread writes: its own puts and everything else queued.
    pub fn wait(self) -> Result<(), StoreError> {
        self.0.wait().map(drop)
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
) -> Vec<(Arc<Slot>, Answer)> {
    let (requests, slots): (Vec<_>, Vec<_>) = group
        .into_iter()
        .map(|job| (job.request, job.answer))
        .unzip();
    match store.write_requests(requests, true) {
        Ok(answers) => {
            // Published before any answer is left: a writer that finds its
            // answer must be able to read its write.
            let view = Arc::new(store.view());
            *published.write().unwrap_or_else(PoisonError::into_inner) = view;
            slots.into_iter().zip(answers).collect()
        }
        Err(error) => slots
            .into_iter()
            .map(|slot| (slot, Err(copy(&error))))
            .collect(),
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
        StoreError::ReadOnly => StoreError::ReadOnly,
        other => StoreError::Inconsistent(other.to_string()),
    }
}
