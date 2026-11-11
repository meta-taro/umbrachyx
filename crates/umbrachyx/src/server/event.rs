//! Connections served by a few threads, each waiting on many sockets at
//! once, instead of a thread per connection.
//!
//! A thread per connection is woken for every request and put back to
//! sleep after it; with more connections than processors the server spent
//! its time being scheduled. Here a thread asks the operating system which
//! of its sockets have something to read, and takes them in turn:
//!
//! 1. it reads each and carries out the commands, queueing the `SET`s;
//! 2. it sends each connection the replies that are ready.
//!
//! It never waits for a sync. One more thread does: it writes the `SET`s
//! every serving thread queued, as one group with one sync, and tells the
//! serving threads, which then send those replies. A serving thread that
//! waited for the sync itself would keep every other connection it serves
//! waiting too, and a read must never wait for a write. A connection's own
//! commands stay in order: one that is not a `SET` is not carried out
//! until the `SET`s before it are answered, so it reads what they wrote.
//!
//! Reads are answered from a view, on every serving thread at once, which
//! one thread serving every connection could not.
//!
//! A connection whose client does not read its replies is not read from
//! until it does, so the replies waiting for it stay bounded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::TcpStream as StdStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use mio::net::TcpStream;
use mio::{Events, Interest, Poll, Token, Waker};

use super::resp::{self, Reply};
use super::session::Session;
use super::{MAX_PENDING_OUT, READ_CHUNK, Server, Slot, Step, is_set, set_reply};
use crate::store::{Pending, Writer};

/// The token of the waker that announces new connections.
const WAKE: Token = Token(usize::MAX);
/// Events taken from the operating system at a time.
const EVENTS: usize = 1024;
/// Bytes read from one connection before the others get their turn.
const READ_BUDGET: usize = 1024 * 1024;

/// Whether a socket that gave less than was asked for can be left without
/// reading it again until it says it would block. On Unix every arrival is
/// a new event, so the second read, one system call a request, is saved.
/// On Windows a socket is only watched again once a read has said it would
/// block: left after a short read, it is never heard from again.
const HAS_EDGE_ON_ARRIVAL: bool = cfg!(unix);

/// Whether commits are queued for the thread that writes them.
#[derive(Default)]
pub(super) struct Queued {
    some: Mutex<bool>,
    told: Condvar,
}

impl Queued {
    fn tell(&self) {
        *self.some.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.told.notify_one();
    }

    /// Sleeps until told.
    fn wait(&self) {
        let mut some = self.some.lock().unwrap_or_else(PoisonError::into_inner);
        while !*some {
            some = self.told.wait(some).unwrap_or_else(PoisonError::into_inner);
        }
        *some = false;
    }
}

/// Starts the thread that writes the commits the serving threads queue,
/// and wakes them once each group is durable.
pub(super) fn spawn_writer(
    writer: Writer,
    queued: Arc<Queued>,
    wakers: Vec<Arc<Waker>>,
) -> io::Result<()> {
    thread::Builder::new()
        .name("resp commits".to_owned())
        .spawn(move || {
            loop {
                queued.wait();
                writer.write_queued();
                for waker in &wakers {
                    // A failed wake leaves the replies for the next one.
                    let _ = waker.wake();
                }
            }
        })
        .map(drop)
}

/// One thread's end of the server: where new connections are handed to it.
pub(super) struct Handle {
    incoming: Sender<(StdStream, Slot)>,
    waker: Arc<Waker>,
}

impl Handle {
    pub(super) fn waker(&self) -> Arc<Waker> {
        Arc::clone(&self.waker)
    }

    /// Hands a connection to the thread. If the thread is gone, the
    /// connection is dropped, which closes it.
    pub(super) fn give(&self, stream: StdStream, slot: Slot) {
        if self.incoming.send((stream, slot)).is_ok() {
            // A failed wake leaves the connection waiting for the next one.
            let _ = self.waker.wake();
        }
    }
}

/// Starts a thread that serves the connections given to it.
pub(super) fn spawn(server: Server, number: usize) -> io::Result<Handle> {
    let poll = Poll::new()?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let (incoming, received) = mpsc::channel();
    let mut serving = Loop {
        server,
        poll,
        incoming: received,
        connections: HashMap::new(),
        awaiting: HashSet::new(),
        next_token: 0,
        chunk: vec![0; READ_CHUNK],
    };
    thread::Builder::new()
        .name(format!("resp loop {number}"))
        .spawn(move || serving.run())?;
    Ok(Handle { incoming, waker })
}

struct Connection {
    stream: TcpStream,
    session: Session,
    /// Bytes read and not yet parsed into whole commands.
    input: Vec<u8>,
    /// Replies not yet sent, from `sent` on.
    out: Vec<u8>,
    sent: usize,
    /// `SET`s queued and not yet answered, in order, and any reply that
    /// has to go out after one of them.
    unanswered: VecDeque<Outgoing>,
    /// Close once the replies are sent: the client hung up, or sent
    /// something that is not the protocol.
    is_closing: bool,
    /// The socket would not take more; nothing is read until it does.
    is_blocked: bool,
    /// Whole commands may be waiting in `input`: carrying them out stopped
    /// for the replies to be sent first.
    has_backlog: bool,
    /// A command that is not a `SET` waits in `input` for the `SET`s
    /// before it to be answered.
    is_waiting: bool,
    _slot: Slot,
}

/// A reply on its way, in the order its command came.
enum Outgoing {
    /// A `SET` being written.
    Waiting(Pending),
    /// A reply held back behind one.
    Ready(Reply),
}

/// What became of a connection after its replies were sent.
enum Sent {
    /// Everything went out.
    All,
    /// The socket is full; the rest goes when it can take more.
    Part,
    /// The connection is done, or broken.
    Close,
}

struct Loop {
    server: Server,
    poll: Poll,
    incoming: Receiver<(StdStream, Slot)>,
    connections: HashMap<usize, Connection>,
    /// The connections with `SET`s being written.
    awaiting: HashSet<usize>,
    next_token: usize,
    /// What a read fills, shared by every connection.
    chunk: Vec<u8>,
}

impl Loop {
    fn run(&mut self) {
        let mut events = Events::with_capacity(EVENTS);
        // Connections with more to read than their turn allowed, or with
        // commands held back; they are taken again without waiting.
        let mut again: Vec<usize> = Vec::new();
        loop {
            let timeout = (!again.is_empty()).then_some(Duration::ZERO);
            if let Err(error) = self.poll.poll(&mut events, timeout) {
                if error.kind() != io::ErrorKind::Interrupted {
                    eprintln!("umbrachyx: waiting for connections: {error}");
                }
                continue;
            }
            let mut ready = std::mem::take(&mut again);
            for event in &events {
                if event.token() == WAKE {
                    // New connections, or commits written: either way the
                    // connections with SETs out are worth another look.
                    self.adopt(&mut ready);
                    ready.extend(&self.awaiting);
                } else {
                    ready.push(event.token().0);
                }
            }
            ready.sort_unstable();
            ready.dedup();
            self.take_turns(&ready, &mut again);
        }
    }

    /// Delivers what was written, reads, and replies, for each ready
    /// connection.
    fn take_turns(&mut self, ready: &[usize], again: &mut Vec<usize>) {
        let mut has_queued = false;
        for token in ready {
            if let Some(connection) = self.connections.get_mut(token) {
                connection.deliver();
                let before = connection.unanswered.len();
                connection.read(&self.server, &mut self.chunk);
                has_queued |= connection.unanswered.len() > before;
            }
        }
        if has_queued {
            self.server.queued.tell();
        }
        for token in ready {
            let Some(connection) = self.connections.get_mut(token) else {
                continue;
            };
            if connection.unanswered.is_empty() {
                self.awaiting.remove(token);
            } else {
                self.awaiting.insert(*token);
            }
            let was_blocked = connection.is_blocked;
            let is_done = connection.is_closing && connection.unanswered.is_empty();
            match connection.send() {
                Sent::All if is_done => self.close(*token),
                Sent::All => {
                    connection.is_blocked = false;
                    if was_blocked && !self.listen(*token, Interest::READABLE) {
                        continue;
                    }
                    let connection = &self.connections[token];
                    if was_blocked || connection.has_backlog {
                        again.push(*token);
                    }
                }
                Sent::Part => {
                    connection.is_blocked = true;
                    if !was_blocked {
                        self.listen(*token, Interest::WRITABLE);
                    }
                }
                Sent::Close => self.close(*token),
            }
        }
    }

    /// Takes the connections handed over since the last wake.
    fn adopt(&mut self, ready: &mut Vec<usize>) {
        while let Ok((stream, slot)) = self.incoming.try_recv() {
            let mut stream = TcpStream::from_std(stream);
            let token = self.next_token;
            self.next_token = (self.next_token + 1) % WAKE.0;
            let registered =
                self.poll
                    .registry()
                    .register(&mut stream, Token(token), Interest::READABLE);
            if let Err(error) = registered {
                eprintln!("umbrachyx: a connection could not be watched: {error}");
                continue;
            }
            self.connections.insert(
                token,
                Connection {
                    stream,
                    session: Session::new(),
                    input: Vec::new(),
                    out: Vec::new(),
                    sent: 0,
                    unanswered: VecDeque::new(),
                    is_closing: false,
                    is_blocked: false,
                    has_backlog: false,
                    is_waiting: false,
                    _slot: slot,
                },
            );
            // It may have sent something before it was watched.
            ready.push(token);
        }
    }

    /// Changes what a connection is watched for. Closes it and answers
    /// `false` if that fails.
    fn listen(&mut self, token: usize, interest: Interest) -> bool {
        let Some(connection) = self.connections.get_mut(&token) else {
            return false;
        };
        let watched =
            self.poll
                .registry()
                .reregister(&mut connection.stream, Token(token), interest);
        if watched.is_err() {
            self.close(token);
        }
        watched.is_ok()
    }

    fn close(&mut self, token: usize) {
        self.awaiting.remove(&token);
        if let Some(mut connection) = self.connections.remove(&token) {
            // The socket closes when dropped; not being watched any more
            // only matters if that somehow fails.
            let _ = self.poll.registry().deregister(&mut connection.stream);
        }
    }
}

impl Connection {
    /// Moves to `out` the replies of the `SET`s written since the last
    /// look, and what was held back behind them, as far as they are ready.
    fn deliver(&mut self) {
        while let Some(front) = self.unanswered.front_mut() {
            match front {
                Outgoing::Waiting(pending) => match pending.poll() {
                    Some(answer) => set_reply(answer).write_to(&mut self.out),
                    None => return,
                },
                Outgoing::Ready(reply) => reply.write_to(&mut self.out),
            }
            self.unanswered.pop_front();
        }
    }

    /// Reads what has arrived and carries out the whole commands in it.
    /// Stops at the read budget, and reads nothing while replies wait for
    /// the client to take them, nor while a command waits for the `SET`s
    /// before it.
    fn read(&mut self, server: &Server, chunk: &mut [u8]) {
        if self.is_blocked || self.is_closing {
            return;
        }
        if self.has_backlog || self.is_waiting {
            self.execute(server);
        }
        let mut budget = READ_BUDGET;
        while !self.has_backlog && !self.is_waiting && !self.is_closing && budget > 0 {
            match self.stream.read(chunk) {
                Ok(0) => self.is_closing = true,
                Ok(read) => {
                    self.input.extend_from_slice(&chunk[..read]);
                    self.execute(server);
                    if HAS_EDGE_ON_ARRIVAL && read < chunk.len() {
                        // The socket had no more: it will say when it does.
                        return;
                    }
                    budget = budget.saturating_sub(read);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    // Broken: there is nobody to send the replies to.
                    self.out.clear();
                    self.sent = 0;
                    self.unanswered.clear();
                    self.is_closing = true;
                }
            }
        }
        // Out of budget with the socket not drained: no new event will
        // come for what is already there.
        self.has_backlog |= budget == 0;
    }

    /// Carries out the whole commands in `input`. Stops when the replies
    /// waiting to be sent are many enough that they should go first, and
    /// at a command that is not a `SET` while `SET`s before it are still
    /// being written: it must read what they wrote.
    fn execute(&mut self, server: &Server) {
        self.has_backlog = false;
        self.is_waiting = false;
        let mut used = 0;
        let result = loop {
            if self.out.len() - self.sent >= MAX_PENDING_OUT {
                self.has_backlog = true;
                break Ok(());
            }
            match resp::parse(&self.input[used..]) {
                Ok(Some((command, _))) if !is_set(&command) && !self.unanswered.is_empty() => {
                    self.is_waiting = true;
                    break Ok(());
                }
                Ok(Some((command, len))) => {
                    used += len;
                    match server.execute(&command, &mut self.session) {
                        Some(Step::Sent(commit)) => {
                            self.unanswered.push_back(Outgoing::Waiting(commit));
                        }
                        Some(Step::Reply(reply)) => self.reply(reply),
                        None => {}
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        self.input.drain(..used);
        if let Err(error) = result {
            self.reply(Reply::Error(format!("ERR {error}")));
            self.is_closing = true;
        }
    }

    /// Sends `reply` now, or after the `SET`s still being written.
    fn reply(&mut self, reply: Reply) {
        if self.unanswered.is_empty() {
            reply.write_to(&mut self.out);
        } else {
            self.unanswered.push_back(Outgoing::Ready(reply));
        }
    }

    /// Sends the replies waiting, as far as the socket takes them.
    fn send(&mut self) -> Sent {
        while self.sent < self.out.len() {
            match self.stream.write(&self.out[self.sent..]) {
                Ok(0) => return Sent::Close,
                Ok(written) => self.sent += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Sent::Part,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Sent::Close,
            }
        }
        self.out.clear();
        self.sent = 0;
        Sent::All
    }
}
