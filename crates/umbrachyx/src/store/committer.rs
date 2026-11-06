//! Group commit: one thread owns the store and writes, with one sync, every
//! commit that arrived while the previous sync was running.
//!
//! A sync costs the same for one commit as for many, so writers that would
//! each wait for their own sync share one instead. Each writer is still
//! answered only once its commit is durable.
//!
//! Readers on other threads read a [`View`] the committer publishes after
//! each group, before it answers the group's writers, so a write is seen by
//! every reader by the time its writer hears it is durable.
//!
//! Questions about the history, which a view does not hold, are sent down
//! the same channel as commits and answered on the committer's thread, so
//! a question sees every commit sent before it.

use std::io;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{Clock, CommitId, CommitRequest, Inspect, Provenance, Store, StoreError, View};
use crate::log::record::{GROUP_OVERHEAD, commit_len};
use crate::log::{Edit, LogError, MAX_PAYLOAD_LEN, Storage};

type Answer = Result<CommitId, StoreError>;

/// When several writers are active, how long the committer waits for one
/// more commit before it syncs. The writers it just answered need a moment
/// to send their next commit; syncing without them would split the writers
/// into two groups taking turns, and double everyone's wait.
const QUIET_GAP: Duration = Duration::from_micros(50);
/// The longest it gathers, however steadily commits keep arriving.
const MAX_GATHER: Duration = Duration::from_micros(500);

enum Message {
    Commit(Job),
    Ask(Question),
    Stop,
}

/// Reads the store and sends the answer back.
type Question = Box<dyn FnOnce(&dyn Inspect) + Send>;

struct Job {
    request: CommitRequest,
    /// The bytes the request adds to a group's payload.
    len: usize,
    reply: SyncSender<Answer>,
}

/// Owns a store on its own thread and groups the commits sent to it.
pub struct Committer<S: Storage, C: Clock> {
    messages: Sender<Message>,
    published: Published,
    thread: Option<JoinHandle<Store<S, C>>>,
}

/// The latest view. Replacing it swaps one pointer, so readers wait on the
/// lock no longer than that.
type Published = Arc<RwLock<Arc<View>>>;

/// Sends commits to a [`Committer`]. Cheap to clone, one per writer thread.
#[derive(Clone)]
pub struct Writer {
    messages: Sender<Message>,
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
        let (messages, received) = mpsc::channel();
        let published = Arc::new(RwLock::new(Arc::new(store.view())));
        let publish_to = Arc::clone(&published);
        let thread = thread::spawn(move || run(store, &received, &publish_to));
        Self {
            messages,
            published,
            thread: Some(thread),
        }
    }

    pub fn writer(&self) -> Writer {
        Writer {
            messages: self.messages.clone(),
        }
    }

    pub fn reader(&self) -> Reader {
        Reader {
            published: Arc::clone(&self.published),
        }
    }

    /// Answers every commit sent before this call, then gives the store
    /// back. Commits sent afterwards are refused with `Stopped`.
    pub fn stop(mut self) -> Store<S, C> {
        let thread = self.thread.take().expect("only stop and drop take it");
        // The thread runs until it reads this, so the send fails only if
        // the thread panicked, and join passes that panic on.
        let _ = self.messages.send(Message::Stop);
        thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }
}

impl<S: Storage, C: Clock> Drop for Committer<S, C> {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.messages.send(Message::Stop);
            // A panic on the committer thread was already reported there.
            let _ = thread.join();
        }
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

    /// Sends a commit without waiting for it, so commits sent one after
    /// another can share a sync. They are written in the order sent.
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
        let (reply, answer) = mpsc::sync_channel(1);
        // If the committer has stopped, the job comes back in the error and
        // is dropped with its reply, so waiting gives `Stopped`.
        let _ = self.messages.send(Message::Commit(Job {
            request,
            len,
            reply,
        }));
        Pending { answer }
    }

    /// Answers `question` from the store once the commits sent before it
    /// are written. It waits behind their sync, so it suits a person
    /// reading the history, not a hot path; reads of live values go
    /// through a [`Reader`].
    pub fn ask<T: Send + 'static>(
        &self,
        question: impl FnOnce(&dyn Inspect) -> T + Send + 'static,
    ) -> Result<T, StoreError> {
        let (reply, answer) = mpsc::sync_channel(1);
        let question: Question = Box::new(move |store| {
            // The asker stopped waiting; nobody is left to tell.
            let _ = reply.send(question(store));
        });
        // As for a commit: if the committer has stopped, the question is
        // dropped with its reply, and waiting gives `Stopped`.
        let _ = self.messages.send(Message::Ask(question));
        answer.recv().map_err(|_| StoreError::Stopped)
    }
}

/// A commit sent to a [`Committer`] and not yet answered.
pub struct Pending {
    answer: Receiver<Answer>,
}

impl Pending {
    /// The commit's id once it is durable.
    pub fn wait(self) -> Result<CommitId, StoreError> {
        // The reply is dropped unanswered only if the committer stopped
        // before reading the job.
        self.answer.recv().map_err(|_| StoreError::Stopped)?
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

fn run<S: Storage, C: Clock>(
    mut store: Store<S, C>,
    received: &Receiver<Message>,
    published: &Published,
) -> Store<S, C> {
    let mut carried = None;
    let mut is_busy = false;
    loop {
        let message = carried
            .take()
            .unwrap_or_else(|| received.recv().unwrap_or(Message::Stop));
        let first = match message {
            Message::Commit(job) => job,
            Message::Ask(question) => {
                question(&store);
                continue;
            }
            Message::Stop => return store,
        };
        let (group, next) = gather(first, received, is_busy);
        is_busy = group.len() > 1;
        write_group(&mut store, group, published);
        carried = next;
    }
}

/// `first` and every job already waiting, as long as their payload fits in
/// one record; when `is_busy`, also the jobs that keep arriving, as set out
/// by `QUIET_GAP` and `MAX_GATHER`. Returns the group, and the message that
/// ended it: a job that did not fit, a question, or a stop, each of which
/// must wait for the group to be written.
fn gather(first: Job, received: &Receiver<Message>, is_busy: bool) -> (Vec<Job>, Option<Message>) {
    let deadline = Instant::now() + MAX_GATHER;
    let next = || {
        if !is_busy {
            return received.try_recv().ok();
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(QUIET_GAP);
        received.recv_timeout(wait).ok()
    };
    let mut len = GROUP_OVERHEAD + first.len;
    let mut group = vec![first];
    while let Some(message) = next() {
        let Message::Commit(job) = message else {
            return (group, Some(message));
        };
        if len + job.len > MAX_PAYLOAD_LEN {
            return (group, Some(Message::Commit(job)));
        }
        len += job.len;
        group.push(job);
    }
    (group, None)
}

fn write_group<S: Storage, C: Clock>(
    store: &mut Store<S, C>,
    group: Vec<Job>,
    published: &Published,
) {
    let (requests, replies): (Vec<_>, Vec<_>) = group
        .into_iter()
        .map(|job| (job.request, job.reply))
        .unzip();
    // A writer that stopped waiting is no reason to stop answering others.
    match store.commit_all(requests) {
        Ok(answers) => {
            let view = Arc::new(store.view());
            *published.write().unwrap_or_else(PoisonError::into_inner) = view;
            for (reply, answer) in replies.into_iter().zip(answers) {
                let _ = reply.send(answer);
            }
        }
        Err(error) => {
            for reply in replies {
                let _ = reply.send(Err(copy(&error)));
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
