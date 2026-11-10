//! A server that speaks the Redis protocol, so existing clients and
//! benchmarks can read and write a branch.
//!
//! It serves one branch, never main: writes from a client are not trusted,
//! so they go where a person can review them before merging. Each `SET` is
//! a commit, acknowledged once durable, with the client's address as its
//! author. Reads come from the latest view the committer published, so a
//! client reads back what it has written.
//!
//! For a viewer, `SCAN`, `TYPE` and `TTL` list and describe the keys of the
//! branch (see `keys`), and commands named `UMBRA.` read the history of any
//! branch and the tables declared (see `history`).
//!
//! There is no authentication yet, so a server should listen only on this
//! machine.
//!
//! Each connection has a thread. Commands sent together (pipelined) are
//! answered together, in order. SETs sent together are written together,
//! sharing syncs, and any other command waits for the SETs before it, so a
//! GET reads what the same client set just before.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::log::Edit;
use crate::store::{CommitId, MAIN_BRANCH, Pending, Provenance, Reader, StoreError, Writer};

mod history;
mod keys;
pub mod resp;

pub use history::MAX_PAGE;
use resp::{Command, Reply};

/// Connections served at once unless set otherwise. One more is told so
/// and closed.
pub const MAX_CONNECTIONS: usize = 1024;
const READ_CHUNK: usize = 64 * 1024;
/// Replies are sent once this much is waiting, so a client that pipelines
/// many reads of large values does not make the server hold them all.
const MAX_PENDING_OUT: usize = 1024 * 1024;
/// How much of an unknown command's name is repeated in the error.
const MAX_NAME_SHOWN: usize = 64;
/// After `accept` fails, for example because no file descriptor is free,
/// the wait before trying again.
const ACCEPT_RETRY: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum ServerError {
    /// Clients may not write to main.
    Main,
    UnknownBranch(String),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Main => write!(f, "clients write to a branch, never to {MAIN_BRANCH}"),
            Self::UnknownBranch(name) => write!(f, "no branch named {name}"),
        }
    }
}

impl std::error::Error for ServerError {}

/// Serves one branch of a store a committer owns.
#[derive(Clone)]
pub struct Server {
    branch: Arc<str>,
    writer: Writer,
    reader: Reader,
    connections: Arc<AtomicUsize>,
    max_connections: usize,
}

impl Server {
    pub fn new(branch: &str, writer: Writer, reader: Reader) -> Result<Self, ServerError> {
        if branch == MAIN_BRANCH {
            return Err(ServerError::Main);
        }
        if !reader.view().has_branch(branch) {
            return Err(ServerError::UnknownBranch(branch.to_owned()));
        }
        Ok(Self {
            branch: Arc::from(branch),
            writer,
            reader,
            connections: Arc::new(AtomicUsize::new(0)),
            max_connections: MAX_CONNECTIONS,
        })
    }

    pub fn max_connections(self, max_connections: usize) -> Self {
        Self {
            max_connections,
            ..self
        }
    }

    /// Accepts connections for as long as the process runs. A failure to
    /// accept or to start a connection's thread costs that connection only.
    pub fn serve(self, listener: TcpListener) -> ! {
        loop {
            let (stream, peer) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) => {
                    eprintln!("umbrachyx: accept: {error}");
                    thread::sleep(ACCEPT_RETRY);
                    continue;
                }
            };
            let Some(slot) = Slot::take(&self.connections, self.max_connections) else {
                refuse(stream);
                continue;
            };
            let server = self.clone();
            let started = thread::Builder::new()
                .name(format!("resp {peer}"))
                .spawn(move || server.connection(stream, peer, slot));
            if let Err(error) = started {
                eprintln!("umbrachyx: {peer}: no thread for the connection: {error}");
            }
        }
    }

    fn connection(self, mut stream: TcpStream, peer: SocketAddr, _slot: Slot) {
        // A connection that fails is closed; the server goes on. A client
        // that hangs up is not worth a line of the log.
        match self.talk(&mut stream, peer) {
            Ok(()) => {}
            Err(error) if is_hang_up(&error) => {}
            Err(error) => eprintln!("umbrachyx: {peer}: {error}"),
        }
    }

    /// Reads commands and answers each batch that arrived together.
    fn talk(&self, stream: &mut TcpStream, peer: SocketAddr) -> io::Result<()> {
        stream.set_nodelay(true)?;
        let who = format!("resp {peer}");
        let mut input = Vec::new();
        let mut out = Vec::new();
        let mut unanswered = Vec::new();
        let mut chunk = vec![0; READ_CHUNK];
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(());
            }
            input.extend_from_slice(&chunk[..read]);
            let mut used = 0;
            let result = loop {
                match resp::parse(&input[used..]) {
                    Ok(Some((command, len))) => {
                        used += len;
                        self.answer(&command, &who, &mut unanswered, &mut out);
                        if out.len() >= MAX_PENDING_OUT {
                            stream.write_all(&out)?;
                            out.clear();
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                }
            };
            input.drain(..used);
            settle(&mut unanswered, &mut out);
            if let Err(error) = result {
                Reply::Error(format!("ERR {error}")).write_to(&mut out);
                stream.write_all(&out)?;
                return Ok(());
            }
            stream.write_all(&out)?;
            out.clear();
        }
    }

    /// Carries out one command. Its reply goes to `out`, or, for a SET, to
    /// `unanswered` until the write is durable.
    fn answer(
        &self,
        command: &Command,
        who: &str,
        unanswered: &mut Vec<Pending>,
        out: &mut Vec<u8>,
    ) {
        // A read must see the SETs sent before it.
        if !is_set(command) {
            settle(unanswered, out);
        }
        match self.execute(command, who) {
            Some(Step::Sent(commit)) => unanswered.push(commit),
            Some(Step::Reply(reply)) => {
                settle(unanswered, out);
                reply.write_to(out);
            }
            None => {}
        }
    }

    /// The reply to one command; none to an empty one, as Redis does.
    fn execute(&self, command: &Command, who: &str) -> Option<Step> {
        let (name, args) = command.split_first()?;
        let name = String::from_utf8_lossy(name).to_ascii_uppercase();
        Some(Step::Reply(match (name.as_str(), args) {
            ("PING", []) => Reply::Simple("PONG"),
            ("PING", [message]) => Reply::Bulk(Some(message.clone())),
            ("GET", [key]) => {
                let value = self.reader.view().get(&self.branch, key);
                Reply::Bulk(value.map(|value| value.to_vec()))
            }
            ("SET", [key, value]) => return Some(Step::Sent(self.set(key, value, who))),
            ("SET", [_, _, _, ..]) => Reply::Error("ERR SET options are not supported".to_owned()),
            ("PING" | "GET" | "SET", _) => Reply::Error(format!(
                "ERR wrong number of arguments for '{}' command",
                name.to_ascii_lowercase()
            )),
            _ => {
                let view = self.reader.view();
                if let Some(reply) = keys::answer(&view, &self.branch, &name, args) {
                    return Some(Step::Reply(reply));
                }
                if let Some(reply) = history::answer(&self.writer, &self.branch, &name, args) {
                    return Some(Step::Reply(reply));
                }
                let shown: String = name.chars().take(MAX_NAME_SHOWN).collect();
                Reply::Error(format!("ERR unknown command '{}'", shown.escape_default()))
            }
        }))
    }

    fn set(&self, key: &[u8], value: &[u8], who: &str) -> Pending {
        let edit = Edit {
            key: key.to_vec(),
            value: Some(value.to_vec()),
        };
        let provenance = Provenance {
            who: who.to_owned(),
            why: "SET".to_owned(),
            approved_by: None,
        };
        self.writer.send(&self.branch, vec![edit], provenance)
    }
}

/// Where a command's reply comes from.
enum Step {
    Reply(Reply),
    /// A SET, answered once its write is durable.
    Sent(Pending),
}

fn is_set(command: &Command) -> bool {
    command
        .first()
        .is_some_and(|name| name.eq_ignore_ascii_case(b"SET"))
}

/// Waits for the SETs sent so far and adds their replies to `out`, in the
/// order they were sent.
fn settle(unanswered: &mut Vec<Pending>, out: &mut Vec<u8>) {
    for commit in unanswered.drain(..) {
        set_reply(commit.wait()).write_to(out);
    }
}

fn set_reply(answer: Result<CommitId, StoreError>) -> Reply {
    match answer {
        Ok(_) => Reply::Simple("OK"),
        Err(error) => Reply::Error(format!("ERR {error}")),
    }
}

/// Tells a client past the limit so, and closes its connection.
fn refuse(mut stream: TcpStream) {
    let mut out = Vec::new();
    Reply::Error("ERR max number of clients reached".to_owned()).write_to(&mut out);
    // The client is turned away either way; a failed write changes nothing.
    let _ = stream.write_all(&out);
}

fn is_hang_up(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

/// One of the connections allowed at once, given back when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(connections: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let taken = connections.fetch_add(1, Ordering::SeqCst);
        // Made before the check, so a refused slot is given back too.
        let slot = Self(Arc::clone(connections));
        (taken < max).then_some(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
