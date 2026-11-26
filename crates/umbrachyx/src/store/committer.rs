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

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};
use std::time::Duration;

use super::backup::Origin;
use super::follow::{Feed, Follower};
use super::replicas::Replicas;
use super::unsynced::Aside;
use super::{
    Clock, CommitId, CommitRequest, Inspect, Provenance, Request, Review, Store, StoreError, View,
};
use crate::log::record::{commit_len, edits_len};
use crate::log::{Edit, LogError, Storage};

mod copies;
mod turns;

use turns::{Job, Slot, Turns, lock};

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

/// The store behind the turns, with its storage and clock hidden so that
/// writers need not name them.
trait Engine: Send + Sync {
    fn turns(&self) -> &Turns;
    /// Writes a group and answers it. The caller holds the turn.
    fn write(&self, group: Vec<Job>);
    /// Writes everything queued. The caller holds the turn.
    fn write_queue(&self);
    /// See [`Writer::hold_early`].
    fn hold_early(&self, puts: Vec<Edit>, has_writes_out: bool) -> Result<HeldEarly, Vec<Edit>>;
    /// See [`Writer::puts_held_early`].
    fn puts_held_early(&self) -> u64;
    /// Whether the store was there to ask. The caller holds the turn.
    fn inspect(&self, question: Question<'_>) -> bool;
    /// As `inspect`, without writing the queue first and without the
    /// turn: for a question asked while whoever has the turn waits for a
    /// replica.
    fn look(&self, question: Question<'_>) -> bool;
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
    /// How many of them it held once its last erasure was written, as
    /// of the last write, or `None` if nothing was erased.
    fn erased_through(&self) -> Option<u64>;
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
    /// How many records the log held once its last erasure was written,
    /// or 0 if nothing was erased: no erasure is the log's 0th record.
    erased_through: AtomicU64,
    /// What replicas hold, if a write is answered only once one holds
    /// it.
    replicas: Option<Replicas>,
    /// How long a thread that must not sleep looks for the turn.
    look_for_the_turn: Duration,
    /// How many puts were held by the thread that read them.
    held_early: AtomicU64,
    /// Where puts are held while the ones before them are being
    /// synced, which holds the store for as long as the disk takes;
    /// `None` at any other time.
    aside: Mutex<Option<Aside>>,
    /// Whether `aside` holds one, so that asking costs no lock.
    is_aside_open: AtomicBool,
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

    /// See [`Writer::hold_early`].
    fn hold_early(&self, puts: Vec<Edit>, has_writes_out: bool) -> Result<HeldEarly, Vec<Edit>> {
        if !self.answers_puts_early() {
            return Err(puts);
        }
        // Puts of a connection whose earlier writes are still queued
        // must not be applied ahead of them.
        let puts = if has_writes_out {
            puts
        } else {
            match self.hold_aside(puts) {
                Ok(held) => return Ok(held),
                Err(puts) => puts,
            }
        };
        let Some(_turn) = self.turns.take_if_idle(self.look_for_the_turn) else {
            return Err(puts);
        };
        let mut store = lock(&self.store);
        let Some(store) = store.as_mut() else {
            return Err(puts);
        };
        let count = puts.len() as u64;
        let had_unsynced = store.has_unsynced_puts();
        // No view is published: these puts make no key, and the map
        // they change is the one every view reads already.
        let held = match store.hold_sealed(puts) {
            Ok(None) => HeldEarly::Held {
                is_first: !had_unsynced,
            },
            Ok(Some(puts)) => return Err(puts),
            Err(error) => HeldEarly::Refused(error),
        };
        if matches!(held, HeldEarly::Held { .. }) {
            self.held_early.fetch_add(count, Ordering::Relaxed);
            self.has_unsynced_puts.store(true, Ordering::Release);
        }
        Ok(held)
    }

    /// Whether puts may be answered before they are written. A store
    /// that follows refuses writes, and one that waits for replicas may
    /// have to: both are for the thread that writes to say.
    fn answers_puts_early(&self) -> bool {
        !self.is_following && self.replicas.is_none()
    }

    /// Holds `puts` beside the store while it syncs the puts before
    /// them, or gives them back if it is not syncing.
    fn hold_aside(&self, puts: Vec<Edit>) -> Result<HeldEarly, Vec<Edit>> {
        if !self.is_aside_open.load(Ordering::Acquire) {
            return Err(puts);
        }
        let count = puts.len() as u64;
        let mut aside = lock(&self.aside);
        let Some(open) = aside.as_mut() else {
            return Err(puts);
        };
        match open.hold(puts) {
            Some(puts) => Err(puts),
            None => {
                self.held_early.fetch_add(count, Ordering::Relaxed);
                // Whoever syncs looks again when its sync ends.
                Ok(HeldEarly::Held { is_first: false })
            }
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
        self.erased_through
            .store(store.erased_through().unwrap_or(0), Ordering::Release);
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

    /// Writes the puts held ahead of the disk. With `takes_more`, the
    /// puts that come while the disk takes them are held beside the
    /// store and are in it, not yet written, when this returns.
    fn sync_puts(&self, takes_more: bool) -> Result<(), StoreError> {
        self.write_queue();
        let mut store = lock(&self.store);
        let store = store.as_mut().ok_or(StoreError::Stopped)?;
        if takes_more && self.answers_puts_early() && store.has_unsynced_puts() {
            let mut aside = lock(&self.aside);
            *aside = store.aside();
            self.is_aside_open.store(aside.is_some(), Ordering::Release);
        }
        let synced = store.sync_puts();
        // Closed before the store is let go: a put that finds it shut
        // waits for the store, and so comes after the ones taken in.
        self.is_aside_open.store(false, Ordering::Release);
        if let Some(aside) = lock(&self.aside).take() {
            store.take_in(aside);
        }
        self.note_unsynced(store);
        synced
    }

    /// Writes what is queued, and the puts answered before they were on
    /// disk, and takes the store out.
    fn stop(&self) -> Option<Store<S, C>> {
        let _turn = self.turns.take();
        // If the log refuses them, it refuses every write from then on:
        // whoever takes the store finds that out from the store itself.
        let _ = self.sync_puts(false);
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

    fn hold_early(&self, puts: Vec<Edit>, has_writes_out: bool) -> Result<HeldEarly, Vec<Edit>> {
        Core::hold_early(self, puts, has_writes_out)
    }

    fn puts_held_early(&self) -> u64 {
        self.held_early.load(Ordering::Relaxed)
    }

    fn inspect(&self, question: Question<'_>) -> bool {
        self.write_queue();
        self.look(question)
    }

    fn look(&self, question: Question<'_>) -> bool {
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

    fn erased_through(&self) -> Option<u64> {
        Some(self.erased_through.load(Ordering::Acquire)).filter(|&at| at != 0)
    }

    fn has_unsynced_puts(&self) -> bool {
        self.has_unsynced_puts.load(Ordering::Acquire)
    }

    fn sync_puts(&self) -> Result<(), StoreError> {
        Core::sync_puts(self, true)
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
        let erased_through = store.erased_through().unwrap_or(0);
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
                erased_through: AtomicU64::new(erased_through),
                replicas,
                look_for_the_turn: look_for_the_turn_with(
                    std::thread::available_parallelism().map_or(1, usize::from),
                ),
                held_early: AtomicU64::new(0),
                aside: Mutex::new(None),
                is_aside_open: AtomicBool::new(false),
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
            self.engine.turns().queue(Job {
                waits_for_the_disk: !matches!(request, Request::SealedPut(_)),
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
        let _turn = self.engine.turns().take_to_write_the_queue();
        self.engine.write_queue();
    }

    /// As [`Writer::put`], for puts [`Writer::hold_early`] gave back:
    /// their values are as the store holds them already.
    pub fn put_sealed(&self, edits: Vec<Edit>) -> PendingPut {
        let len = edits_len(&edits);
        PendingPut(self.queue(Request::SealedPut(edits), len))
    }

    /// Applies `puts` and holds them for the log, on this thread, and
    /// says so at once: for puts to plain-async tables, whose values
    /// [`View::held_early`](super::View::held_early) made ready. For a
    /// thread that serves connections: handing such puts to the thread
    /// that writes and being woken with the answer costs more than
    /// applying them.
    ///
    /// It never waits for the disk, and looks for another thread's turn
    /// to end no longer than a few turns take. The puts come back, for
    /// [`Writer::put_sealed`], if they would have to wait: somebody is
    /// writing, something is queued that they must not go ahead of, or
    /// the store holds as much ahead of the disk as it may.
    ///
    /// While the puts held earlier are being synced, the store is held
    /// for as long as the disk takes; puts are then held beside it and
    /// answered all the same, unless `has_writes_out`: the caller has
    /// writes queued that these must not be applied ahead of.
    pub fn hold_early(
        &self,
        puts: Vec<Edit>,
        has_writes_out: bool,
    ) -> Result<HeldEarly, Vec<Edit>> {
        self.engine.hold_early(puts, has_writes_out)
    }

    /// How many puts [`Writer::hold_early`] has held since the committer
    /// started.
    pub fn puts_held_early(&self) -> u64 {
        self.engine.puts_held_early()
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
    /// on disk, and returns once they are durable. Puts that
    /// [`Writer::hold_early`] answers meanwhile are not among them:
    /// [`Writer::has_unsynced_puts`] says afterwards whether there are
    /// any. An error means memory
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
    ///
    /// One case differs, in a store that answers a write only once a
    /// replica holds it. While a write waits for a replica, the question
    /// is answered from what is written by then, that write included,
    /// and commits queued behind it are not waited for: a replica that
    /// has gone would hold every question as long as it holds the write.
    pub fn ask<T>(&self, question: impl FnOnce(&dyn Inspect) -> T) -> Result<T, StoreError> {
        let mut answer = None;
        let question: Question<'_> = Box::new(|store| answer = Some(question(store)));
        match self.engine.turns().take_to_read() {
            Some(_turn) => self.engine.inspect(question),
            None => self.engine.look(question),
        };
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

/// What [`Writer::hold_early`] did with the puts it kept.
#[derive(Debug)]
pub enum HeldEarly {
    /// They are applied, and every reader sees them. `is_first` if they
    /// are the first the log does not hold: from now somebody has to
    /// call [`Writer::sync_puts`] in time.
    Held { is_first: bool },
    /// The store refused them.
    Refused(StoreError),
}

/// How long a thread that must not sleep looks for the turn to write,
/// on a machine with more than one processor. A turn that holds puts for
/// one connection takes a few microseconds, so this is several turns of
/// the other serving threads. Too short, and with two serving threads
/// four puts in five went to the thread that writes after all, which
/// then held the turn for all of them; measured at 20, 100, 500 and
/// 2,000 microseconds, 100 gave the most writes.
const LOOK_FOR_THE_TURN: Duration = Duration::from_micros(100);

/// With one processor, the thread that has the turn cannot end it while
/// this one looks.
fn look_for_the_turn_with(processors: usize) -> Duration {
    if processors > 1 {
        LOOK_FOR_THE_TURN
    } else {
        Duration::ZERO
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
                queue = turns.wait(queue);
                continue;
            }
            {
                let _turn = turns.take_found_free(queue);
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
