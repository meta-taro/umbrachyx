//! Group commit: one thread owns the store and writes, with one sync, every
//! commit that arrived while the previous sync was running.
//!
//! A sync costs the same for one commit as for many, so writers that would
//! each wait for their own sync share one instead. Each writer is still
//! answered only once its commit is durable.

use std::io;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{Clock, CommitId, CommitRequest, Provenance, Store, StoreError};
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
    Stop,
}

struct Job {
    request: CommitRequest,
    /// The bytes the request adds to a group's payload.
    len: usize,
    reply: SyncSender<Answer>,
}

/// Owns a store on its own thread and groups the commits sent to it.
pub struct Committer<S: Storage, C: Clock> {
    messages: Sender<Message>,
    thread: Option<JoinHandle<Store<S, C>>>,
}

/// Sends commits to a [`Committer`]. Cheap to clone, one per writer thread.
#[derive(Clone)]
pub struct Writer {
    messages: Sender<Message>,
}

impl<S, C> Committer<S, C>
where
    S: Storage + Send + 'static,
    C: Clock + Send + 'static,
{
    pub fn start(store: Store<S, C>) -> Self {
        let (messages, received) = mpsc::channel();
        let thread = thread::spawn(move || run(store, &received));
        Self {
            messages,
            thread: Some(thread),
        }
    }

    pub fn writer(&self) -> Writer {
        Writer {
            messages: self.messages.clone(),
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
        self.messages
            .send(Message::Commit(Job {
                request,
                len,
                reply,
            }))
            .map_err(|_| StoreError::Stopped)?;
        // The reply is dropped unanswered only if the committer stopped
        // before reading the job.
        answer.recv().map_err(|_| StoreError::Stopped)?
    }
}

fn run<S: Storage, C: Clock>(mut store: Store<S, C>, received: &Receiver<Message>) -> Store<S, C> {
    let mut carried = None;
    let mut is_busy = false;
    loop {
        let first = match carried.take() {
            Some(job) => job,
            None => match received.recv() {
                Ok(Message::Commit(job)) => job,
                Ok(Message::Stop) | Err(_) => return store,
            },
        };
        let (group, next, is_stopping) = gather(first, received, is_busy);
        is_busy = group.len() > 1;
        write_group(&mut store, group);
        if is_stopping {
            return store;
        }
        carried = next;
    }
}

/// `first` and every job already waiting, as long as their payload fits in
/// one record; when `is_busy`, also the jobs that keep arriving, as set out
/// by `QUIET_GAP` and `MAX_GATHER`. Returns the group, a job that did not
/// fit, and whether a stop was read.
fn gather(
    first: Job,
    received: &Receiver<Message>,
    is_busy: bool,
) -> (Vec<Job>, Option<Job>, bool) {
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
        let job = match message {
            Message::Commit(job) => job,
            Message::Stop => return (group, None, true),
        };
        if len + job.len > MAX_PAYLOAD_LEN {
            return (group, Some(job), false);
        }
        len += job.len;
        group.push(job);
    }
    (group, None, false)
}

fn write_group<S: Storage, C: Clock>(store: &mut Store<S, C>, group: Vec<Job>) {
    let (requests, replies): (Vec<_>, Vec<_>) = group
        .into_iter()
        .map(|job| (job.request, job.reply))
        .unzip();
    // A writer that stopped waiting is no reason to stop answering others.
    match store.commit_all(requests) {
        Ok(answers) => {
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
