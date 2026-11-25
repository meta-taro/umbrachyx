//! A server that speaks the Redis protocol, so existing clients and
//! benchmarks can read and write a branch.
//!
//! It serves one branch, never main: writes from a client are not trusted,
//! so they go where a person can review them before merging. Each `SET` is
//! a commit, acknowledged once durable. Its author is the holder of the
//! capability token the connection sent with `AUTH`, which also bounds what
//! it may write, or `unauthenticated` (see `session`). Reads come from the
//! latest view the committer published, so a client reads back what it has
//! written.
//!
//! For a viewer, `SCAN`, `TYPE` and `TTL` list and describe the keys of the
//! branch (see `keys`), and commands named `UMBRA.` read the history of any
//! branch and the tables declared (see `history`).
//!
//! Reads need no token, and writes without one are taken unless the server
//! requires authentication, so a server should listen only on this machine,
//! or where the network itself decides who reaches the port.
//!
//! The server can keep a backup of the store up to date as it serves, in
//! a place named by whoever starts it (see `backups`).
//!
//! A server can hand its log to replicas, and a server can follow another
//! and answer reads only (see `replication`).
//!
//! A few threads serve all the connections, each waiting on many sockets
//! at once (see `event`). Commands sent together (pipelined) are answered
//! together, in order. SETs sent together are written together,
//! sharing syncs, and any other command waits for the SETs before it, so a
//! GET reads what the same client set just before.

use std::fmt;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use biscuit_auth::PublicKey;

use crate::log::Edit;
use crate::store::{MAIN_BRANCH, Pending, PendingPut, Reader, StoreError, Writer};

mod backups;
mod event;
mod history;
mod keys;
mod replication;
pub mod resp;
mod review;
mod session;
mod tls;

pub use history::MAX_PAGE;
use resp::{Command, Reply};
use session::Session;
pub use session::UNAUTHENTICATED;
pub use tls::{Identity, TlsError};

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

/// How long a put to a plain-async table may wait for the disk unless
/// the server is told otherwise: a second, what Redis and Valkey call
/// `appendfsync everysec`, so the two promise the same.
pub const DEFAULT_SYNC_EVERY: Duration = Duration::from_secs(1);

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
    /// The database's root public key, which `AUTH` checks tokens against.
    public_key: Option<PublicKey>,
    /// Refuse writes from connections that have not authenticated.
    require_auth: bool,
    /// Threads serving connections.
    loops: usize,
    /// Tells the thread that writes commits that some are queued.
    queued: Arc<event::Queued>,
    /// The longest a put to a plain-async table is answered before it is
    /// on disk.
    sync_every: Duration,
    /// Where the store is backed up while it is served, and how often.
    backup: Option<(PathBuf, Duration)>,
    /// Where replicas that ask are sent the log, if anywhere.
    feeds_replicas: Option<Arc<TcpListener>>,
    /// Where the server this one follows is, if it follows one.
    follows: Option<String>,
    /// Whether the store keeps its history, or current values only.
    keeps_history: bool,
    /// With it, replicas are answered and the primary is asked over
    /// TLS, each end showing a certificate, and keys travel.
    identity: Option<tls::Configs>,
    /// How following goes, once the server serves.
    following: Option<replication::Shared>,
    /// How backing up goes, once the server serves.
    backing_up: Option<backups::Shared>,
}

impl Server {
    pub fn new(branch: &str, writer: Writer, reader: Reader) -> Result<Self, ServerError> {
        if branch == MAIN_BRANCH {
            return Err(ServerError::Main);
        }
        if !reader.view().has_branch(branch) {
            return Err(ServerError::UnknownBranch(branch.to_owned()));
        }
        // Asked once: a store is opened to keep its history or not, and
        // stays so.
        let keeps_history = writer.ask(|store| store.keeps_history()).unwrap_or(true);
        Ok(Self {
            branch: Arc::from(branch),
            writer,
            reader,
            connections: Arc::new(AtomicUsize::new(0)),
            max_connections: MAX_CONNECTIONS,
            public_key: None,
            require_auth: false,
            loops: default_loops(),
            queued: Arc::default(),
            sync_every: DEFAULT_SYNC_EVERY,
            backup: None,
            feeds_replicas: None,
            follows: None,
            keeps_history,
            identity: None,
            following: None,
            backing_up: None,
        })
    }

    /// Takes `AUTH` with capability tokens signed by this root key.
    pub fn public_key(self, public_key: PublicKey) -> Self {
        Self {
            public_key: Some(public_key),
            ..self
        }
    }

    /// How many threads serve the connections, at least one.
    pub fn threads(self, threads: usize) -> Self {
        Self {
            loops: threads,
            ..self
        }
    }

    /// Whether a connection must `AUTH` before it may write.
    pub fn require_auth(self, require_auth: bool) -> Self {
        Self {
            require_auth,
            ..self
        }
    }

    /// The longest a `SET` to a plain-async table may be answered before
    /// it is on disk: what a power cut can lose of such a table. Other
    /// tables are never answered first, whatever this is.
    pub fn sync_every(self, sync_every: Duration) -> Self {
        Self { sync_every, ..self }
    }

    /// Keeps a backup of the store in `dir`, brought up to date when
    /// the server starts and then every `every`, while it serves (see
    /// `backups`). The place is whoever starts the server's to name; no
    /// command a client sends reads or changes it.
    pub fn backup_to(self, dir: PathBuf, every: Duration) -> Self {
        Self {
            backup: Some((dir, every)),
            ..self
        }
    }

    /// Answers replicas on `listener`: one that asks is sent the records
    /// of the log, every branch, values still sealed, with no token
    /// asked for. A port of its own, so that it can be closed to all
    /// but the replicas; the port clients use never hands the log out.
    pub fn feed_replicas_on(self, listener: TcpListener) -> Self {
        Self {
            feeds_replicas: Some(Arc::new(listener)),
            ..self
        }
    }

    /// Follows the server at `primary`, a host and a port: asks it for
    /// its records as it serves, and answers reads with what it holds.
    /// The store must have been started to follow
    /// (`Committer::start_following`), so that it takes no writes.
    pub fn follow(self, primary: String) -> Self {
        Self {
            follows: Some(primary),
            ..self
        }
    }

    /// Speaks TLS to replicas and to the server this one follows, with
    /// each end showing a certificate the authority in `identity`
    /// signed. On such a connection a replica is given the keys of
    /// subjects it has not seen; without it nothing between the servers
    /// is encrypted and no key is sent. The port clients use is not
    /// changed by this.
    pub fn replicate_as(self, identity: &Identity) -> Result<Self, TlsError> {
        Ok(Self {
            identity: Some(tls::Configs::new(identity)?),
            ..self
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
    pub fn serve(mut self, listener: TcpListener) -> ! {
        if let Some(primary) = self.follows.clone() {
            let tls = self.identity.as_ref().map(|tls| tls.client.clone());
            let following = replication::spawn(self.writer.clone(), primary, tls)
                .expect("the thread that follows the primary starts");
            self.following = Some(following);
        }
        // Before the serving threads are given their copies of this, so
        // each can say how the backup goes.
        if let Some((dir, every)) = self.backup.clone() {
            let backing_up = backups::spawn(self.writer.clone(), dir, every)
                .expect("the thread that backs the store up starts");
            self.backing_up = Some(backing_up);
        }
        let loops: Vec<event::Handle> = (0..self.loops.max(1))
            .filter_map(|number| match event::spawn(self.clone(), number) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    eprintln!("umbrachyx: a serving thread could not start: {error}");
                    None
                }
            })
            .collect();
        assert!(!loops.is_empty(), "no serving thread could start");
        let wakers = loops.iter().map(event::Handle::waker).collect();
        let queued = Arc::clone(&self.queued);
        event::spawn_writer(self.writer.clone(), queued, wakers, self.sync_every)
            .expect("the thread that writes commits starts");
        if let Some(listener) = self.feeds_replicas.clone() {
            let tls = self.identity.as_ref().map(|tls| tls.server.clone());
            replication::spawn_feed(self.writer.clone(), listener, tls)
                .expect("the thread that answers replicas starts");
        }
        let mut next = 0;
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
            // Replies go out as they are ready, and a serving thread never
            // waits on one socket.
            let prepared = stream
                .set_nodelay(true)
                .and_then(|()| stream.set_nonblocking(true));
            if let Err(error) = prepared {
                eprintln!("umbrachyx: {peer}: {error}");
                continue;
            }
            loops[next % loops.len()].give(stream, slot);
            next = next.wrapping_add(1);
        }
    }

    /// The reply to one command; none to an empty one, as Redis does.
    fn execute(&self, command: &Command, session: &mut Session) -> Option<Step> {
        let (name, args) = command.split_first()?;
        let name = String::from_utf8_lossy(name).to_ascii_uppercase();
        Some(Step::Reply(match (name.as_str(), args) {
            ("PING", []) => Reply::Simple("PONG"),
            ("PING", [message]) => Reply::Bulk(Some(message.clone())),
            ("GET", [key]) => {
                let value = self.reader.view().get(&self.branch, key);
                Reply::Bulk(value.map(|value| value.to_vec()))
            }
            ("AUTH", args) => session.authenticate(self.public_key, args),
            ("SET", [key, value]) => {
                match session.may_write(&self.branch, key, self.require_auth) {
                    Ok(()) => return Some(self.set(key, value, session)),
                    Err(refused) => refused,
                }
            }
            ("SET", [_, _, _, ..]) => Reply::Error("ERR SET options are not supported".to_owned()),
            ("PING" | "GET" | "SET", _) => Reply::Error(format!(
                "ERR wrong number of arguments for '{}' command",
                name.to_ascii_lowercase()
            )),
            _ => {
                if let Some(reply) =
                    review::answer(&self.writer, session, self.public_key, &name, args)
                {
                    return Some(Step::Reply(reply));
                }
                let view = self.reader.view();
                if let Some(reply) = keys::answer(&view, &self.branch, &name, args) {
                    return Some(Step::Reply(reply));
                }
                if let Some(reply) = history::answer(&self.writer, &self.branch, &name, args) {
                    return Some(Step::Reply(reply));
                }
                if let Some(reply) =
                    backups::reply(&self.writer, self.backing_up.as_ref(), &name, args)
                {
                    return Some(Step::Reply(reply));
                }
                if let Some(reply) = replication::reply(
                    &self.writer,
                    self.feeds_replicas.is_some(),
                    self.identity.is_some(),
                    self.keeps_history,
                    self.following.as_ref(),
                    &name,
                    args,
                ) {
                    return Some(Step::Reply(reply));
                }
                let shown: String = name.chars().take(MAX_NAME_SHOWN).collect();
                Reply::Error(format!("ERR unknown command '{}'", shown.escape_default()))
            }
        }))
    }

    /// A commit to the branch served, or a put when the key's table
    /// keeps no history. Tables are declared while no server holds the
    /// log, so the view this reads cannot be behind on them.
    fn set(&self, key: &[u8], value: &[u8], session: &Session) -> Step {
        let edit = Edit {
            key: key.to_vec(),
            value: Some(value.to_vec()),
        };
        if self.reader.view().is_plain(key) {
            return Step::Put(self.writer.put(vec![edit]));
        }
        let provenance = session.provenance("SET");
        Step::Sent(self.writer.send(&self.branch, vec![edit], provenance))
    }
}

/// At most this many serving threads unless asked for more: past a few
/// the client, not the server, is what waits.
const MAX_DEFAULT_LOOPS: usize = 4;

fn default_loops() -> usize {
    loops_for(thread::available_parallelism().map_or(1, usize::from))
}

/// A serving thread for every two processors, at least one. The thread
/// that writes and the clients need processors too, and virtual
/// processors are often two to a core: with a thread for each, the
/// threads spend their time being put to sleep and woken, and 50 clients
/// get two thirds of the `GET`s that two threads on four processors give.
fn loops_for(processors: usize) -> usize {
    (processors / 2).clamp(1, MAX_DEFAULT_LOOPS)
}

/// Where a command's reply comes from.
enum Step {
    Reply(Reply),
    /// A SET, answered once its write is durable.
    Sent(Pending),
    /// A SET to a table that keeps no history, answered the same way.
    Put(PendingPut),
}

fn is_set(command: &Command) -> bool {
    command
        .first()
        .is_some_and(|name| name.eq_ignore_ascii_case(b"SET"))
}

fn set_reply<T>(answer: Result<T, StoreError>) -> Reply {
    match answer {
        Ok(_) => Reply::Simple("OK"),
        Err(error) => match unreplicated(&error) {
            Some(reply) => reply,
            None => Reply::Error(format!("ERR {error}")),
        },
    }
}

/// What a write is answered when the server could not do with it what
/// it does with writes, with a first word of its own: a client can tell
/// a write that was not made from one that is here and on no replica
/// yet.
fn unreplicated(error: &StoreError) -> Option<Reply> {
    match error {
        StoreError::ReadOnly => Some(read_only()),
        StoreError::NoReplica => Some(Reply::Error(format!("NOREPLICA {error}"))),
        StoreError::NotReplicated => Some(Reply::Error(format!("NOTREPLICATED {error}"))),
        _ => None,
    }
}

/// What a write or a decision sent to a server that follows another is
/// answered. The first word is the one Redis and Valkey use, which
/// clients know.
fn read_only() -> Reply {
    Reply::Error(
        "READONLY this server follows another and takes no writes; send them to the server \
         it follows"
            .to_owned(),
    )
}

/// Tells a client past the limit so, and closes its connection.
fn refuse(mut stream: TcpStream) {
    let mut out = Vec::new();
    Reply::Error("ERR max number of clients reached".to_owned()).write_to(&mut out);
    // The client is turned away either way; a failed write changes nothing.
    let _ = stream.write_all(&out);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_the_processors_serve_connections_one_at_least_and_four_at_most() {
        let served: Vec<usize> = [1, 2, 3, 4, 8, 16, 64].map(loops_for).to_vec();

        assert_eq!(served, [1, 1, 1, 2, 4, 4, 4]);
    }
}
