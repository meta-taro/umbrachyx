//! Replication between servers: one that hands its log to replicas, and
//! one that follows another and answers reads.
//!
//! A replica is a client of its primary, on a port kept for replicas. It
//! sends one command, `UMBRA.LOG.AFTER <log> <records> <chain> <max
//! bytes>`, which says what it holds, and is answered with the records
//! after those, or with why it is sent none (see `store::follow`).
//! No token is asked for: the records hold every branch, with values
//! still sealed. So a server listens for replicas only when whoever
//! started it named the port, which only its replicas should reach.
//!
//! A server given an identity (see `tls`) speaks TLS on that port and
//! to its primary, each end showing a certificate. Only then do keys
//! travel: a replica sent records sealed under keys it lacks asks for
//! them with `UMBRA.KEYS <fingerprint>...`. Without an identity nothing
//! is encrypted, that command is not answered and not sent, and a
//! replica stops at the first record whose key it was not given.
//!
//! Each replica has a thread of its own on the primary. An answer waits
//! for a turn between writes, and the threads that serve clients never
//! wait for one: a replica that asks all the time would hold up every
//! client served by the thread that answered it.
//!
//! A server that follows takes no writes: the store refuses them, and
//! the client is told `READONLY`. It listens whether or not its primary
//! can be reached, and answers with what it holds. A thread of its own
//! asks the primary, again as soon as it is answered. A primary holds
//! the ask of a replica that has everything until something is written,
//! a second at most, so a replica is sent a record when it is written,
//! not when it next asks. When no answer comes it
//! says so once and keeps asking; when the primary refuses it, or it
//! cannot take in what it was sent, it says why and stops asking, and
//! goes on answering reads.
//!
//! A server may be started to answer a write only once a replica holds
//! it (see `store::replicas`). A write no replica held in time is
//! answered `NOTREPLICATED`: it is in this server's log. Writes after it
//! are answered `NOREPLICA` and not written, until a replica holds
//! everything.
//!
//! `UMBRA.REPLICATION` says which of these it
//! is, and how far behind, to any client; it does not say where the
//! primary is, which is whoever started the server's to know.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::json;

use super::resp::{self, Reply};
use super::tls::{self, Private};
use crate::keys::Fingerprint;
use crate::store::follow::{
    Ask, Feed, FollowError, KeyAnswer, MAX_SENT, Primary, Progress, Refusal, Sent, answer,
    follow_once,
};
use crate::store::{StoreError, Writer};
use wire::{
    Arrived, KEYS, LOG_AFTER, MAX_KEYS_ASKED, answer_reply, ask_command, keys_command, keys_value,
    read_answer, read_ask, read_keys, unhex,
};

mod wire;

/// How long a replica that connected has to show its certificate, from
/// first byte to last. Its place among the few answered is taken by
/// then, so not long.
const HANDSHAKE: Duration = Duration::from_secs(10);
const REPLICATION: &str = "UMBRA.REPLICATION";
/// How long a replica waits before it asks again after an answer that
/// brought nothing. A primary holds such an answer back, so this only
/// keeps a replica from asking without pause where one does not.
const POLL: Duration = Duration::from_millis(1);
/// How long a primary holds the ask of a replica that holds everything
/// before it answers that there is nothing: until then the answer is
/// whatever is written next, as soon as it is. Shorter than a replica
/// waits for an answer.
const HOLD: Duration = Duration::from_secs(1);
const _: () = assert!(HOLD.as_secs() < PATIENCE.as_secs());
/// How long it waits after an ask that got no answer.
const RETRY: Duration = Duration::from_secs(1);
/// How long it waits to reach the primary, and for each answer.
const PATIENCE: Duration = Duration::from_secs(10);
/// How long a replica's connection may say nothing before the primary
/// closes it. A replica asks every millisecond, and one that went
/// without closing would keep its place among the few answered.
const QUIET: Duration = Duration::from_secs(60);
/// Replicas answered at once. One more is told so and closed.
const MAX_REPLICAS: usize = 16;
pub(super) const ONLY_REPLICAS: &str = "ERR this port answers replicas only: UMBRA.LOG.AFTER";

/// What a server that follows knows of how that is going.
#[derive(Debug, Default)]
pub(super) struct Following {
    /// The last answer taken in.
    progress: Option<Progress>,
    /// Why the last ask got no answer, if it got none.
    unreachable: Option<String>,
    /// Why it no longer asks, if it stopped.
    stopped: Option<String>,
}

pub(super) type Shared = Arc<Mutex<Following>>;

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Following> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the replicas asking now said they hold, each at the last ask of
/// its that was answered. A server knows no other replicas than these:
/// one that is not connected, or stopped asking, is not here.
#[derive(Debug, Default)]
pub(super) struct Asking {
    /// The number the next connection that is answered is given.
    next: u64,
    /// How many records each said it held.
    held: HashMap<u64, u64>,
}

impl Asking {
    /// How many of them said they hold fewer records than the log held
    /// once its last erasure was written: `through`, or `None` if
    /// nothing was erased.
    fn behind(&self, through: Option<u64>) -> usize {
        self.held
            .values()
            .filter(|&&held| through.is_some_and(|through| held < through))
            .count()
    }
}

pub(super) type SharedAsking = Arc<Mutex<Asking>>;

/// One connection's line in [`Asking`]: written at each ask of its that
/// is answered, and removed when the connection ends, however it does.
/// An ask that is refused writes nothing: it comes from another log or
/// from whoever reached the port, and what it says it holds means
/// nothing here.
struct Seat {
    asking: SharedAsking,
    number: Option<u64>,
}

impl Seat {
    fn note(&mut self, held: u64) {
        let mut asking = self.asking.lock().unwrap_or_else(PoisonError::into_inner);
        let number = match self.number {
            Some(number) => number,
            None => {
                asking.next += 1;
                *self.number.insert(asking.next)
            }
        };
        asking.held.insert(number, held);
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        if let Some(number) = self.number {
            let mut asking = self.asking.lock().unwrap_or_else(PoisonError::into_inner);
            asking.held.remove(&number);
        }
    }
}

/// The reply to a replication command a client may send, or `None` if
/// `name` is none. `asking` is `None` on a server that feeds no replica.
#[allow(clippy::too_many_arguments)]
pub(super) fn reply(
    writer: &Writer,
    feeds: bool,
    encrypted: bool,
    keeps_history: bool,
    following: Option<&Shared>,
    asking: Option<&SharedAsking>,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    match name {
        REPLICATION if args.is_empty() => {
            let mut status = status(writer, feeds, encrypted, following);
            // How many records the log held once its last erasure was
            // written: a copy that holds as many holds every erasure's
            // record. And of the replicas asking now, how many said
            // they hold fewer.
            let through = writer.erased_through();
            status["erased_through"] = json!(through);
            if let Some(asking) = asking {
                let asking = asking.lock().unwrap_or_else(PoisonError::into_inner);
                status["replicas_asking"] = json!(asking.held.len());
                status["replicas_asking_behind_an_erasure"] = json!(asking.behind(through));
            }
            // What this server holds of what was written: the history,
            // or only the values as they are now.
            status["holds"] = json!(if keeps_history {
                "history"
            } else {
                "current values"
            });
            Some(Reply::Bulk(Some(status.to_string().into_bytes())))
        }
        REPLICATION => Some(Reply::Error(format!(
            "ERR wrong number of arguments for '{}' command",
            REPLICATION.to_ascii_lowercase()
        ))),
        _ => None,
    }
}

/// Starts the thread that takes replicas' connections on `listener`,
/// and gives each a thread that answers it: over TLS if `tls` is given,
/// and then only those that show a certificate.
pub(super) fn spawn_feed(
    writer: Writer,
    listener: Arc<TcpListener>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> io::Result<SharedAsking> {
    let replicas = Arc::new(AtomicUsize::new(0));
    let asking = SharedAsking::default();
    let told = Arc::clone(&asking);
    thread::Builder::new()
        .name("feed".to_owned())
        .spawn(move || {
            loop {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        eprintln!("umbrachyx: accept from a replica: {error}");
                        thread::sleep(RETRY);
                        continue;
                    }
                };
                let slot = super::Slot::take(&replicas, MAX_REPLICAS);
                let writer = writer.clone();
                let tls = tls.clone();
                let seat = Seat {
                    asking: Arc::clone(&told),
                    number: None,
                };
                let started = thread::Builder::new()
                    .name("feed a replica".to_owned())
                    .spawn(move || match (slot, tls) {
                        (Some(_slot), None) => feed_plain(stream, writer, seat),
                        (Some(_slot), Some(tls)) => feed_private(stream, writer, &tls, seat),
                        (None, None) => super::refuse(stream),
                        // Told nothing: the reason would go out in the
                        // clear, to whoever connected.
                        (None, Some(_)) => {}
                    });
                if let Err(error) = started {
                    eprintln!("umbrachyx: a replica's thread could not start: {error}");
                }
            }
        })
        .map(|_| asking)
}

fn feed_plain(stream: TcpStream, writer: Writer, seat: Seat) {
    // An answer goes out when it is ready, not when more follows.
    let _ = stream.set_nodelay(true);
    if stream.set_read_timeout(Some(QUIET)).is_err() {
        return;
    }
    feed(stream, writer, None, seat);
}

/// Answers a replica once it has shown its certificate. One that shows
/// none the authority signed, or does not speak TLS, is closed without
/// a word.
fn feed_private(stream: TcpStream, writer: Writer, tls: &Arc<rustls::ServerConfig>, seat: Seat) {
    let _ = stream.set_nodelay(true);
    let Ok((stream, private)) = tls::accept(tls, stream, HANDSHAKE) else {
        return;
    };
    let timed = stream
        .sock
        .set_read_timeout(Some(QUIET))
        .and_then(|()| stream.sock.set_write_timeout(Some(QUIET)));
    if timed.is_err() {
        return;
    }
    feed(stream, writer, Some(&private), seat);
}

/// Answers one replica until it goes, or sends what is no command.
/// Keys are given only on a connection that is `private`.
fn feed(
    mut stream: impl Read + Write,
    mut writer: Writer,
    private: Option<&Private>,
    mut seat: Seat,
) {
    let mut input = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let command = match resp::parse(&input) {
            Ok(Some((command, len))) => {
                input.drain(..len);
                command
            }
            Ok(None) => match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    input.extend_from_slice(&chunk[..read]);
                    continue;
                }
            },
            Err(_) => return,
        };
        let mut out = Vec::new();
        feed_reply(&mut writer, &command, private, &mut seat).write_to(&mut out);
        if stream.write_all(&out).is_err() {
            return;
        }
    }
}

fn feed_reply(
    writer: &mut Writer,
    command: &[Vec<u8>],
    private: Option<&Private>,
    seat: &mut Seat,
) -> Reply {
    let Some((name, args)) = command.split_first() else {
        return Reply::Error(ONLY_REPLICAS.to_owned());
    };
    if let Some(_private) = private
        && name.eq_ignore_ascii_case(KEYS.as_bytes())
    {
        return keys_reply(writer, args);
    }
    if !name.eq_ignore_ascii_case(LOG_AFTER.as_bytes()) {
        return Reply::Error(ONLY_REPLICAS.to_owned());
    }
    let ask = match read_ask(args) {
        Ok(ask) => ask,
        Err(error) => return Reply::Error(format!("ERR {error}")),
    };
    match held_answer(writer, &ask) {
        Ok(answer) => {
            if answer.is_ok() {
                seat.note(ask.held.records);
            }
            answer_reply(answer)
        }
        Err(error) => Reply::Error(format!("ERR {error}")),
    }
}

/// What the store says of each key asked for, as one value.
fn keys_reply(writer: &mut Writer, args: &[Vec<u8>]) -> Reply {
    if args.len() > MAX_KEYS_ASKED {
        return Reply::Error(format!(
            "ERR at most {MAX_KEYS_ASKED} keys are asked for at once"
        ));
    }
    let Some(wanted) = args
        .iter()
        .map(|arg| unhex(arg))
        .collect::<Option<Vec<Fingerprint>>>()
    else {
        return Reply::Error("ERR a key is asked for by 16 hex digits".to_owned());
    };
    match Feed::keys(writer, &wanted) {
        Ok(answers) => Reply::Bulk(Some(keys_value(&answers))),
        Err(error) => Reply::Error(format!("ERR {error}")),
    }
}

/// The answer to `ask`, held back for up to [`HOLD`] while there is
/// nothing to send.
fn held_answer(writer: &mut Writer, ask: &Ask) -> Result<Result<Sent, Refusal>, StoreError> {
    let first = answer(writer, ask)?;
    let has_nothing = matches!(&first, Ok(sent) if sent.frames.is_empty());
    if has_nothing && writer.wait_for_more_records(ask.held.records, HOLD) {
        return answer(writer, ask);
    }
    Ok(first)
}

fn status(
    writer: &Writer,
    feeds: bool,
    encrypted: bool,
    following: Option<&Shared>,
) -> serde_json::Value {
    match following {
        None => {
            // For a server that answers a write only once a replica
            // holds it: how many records one has said it holds, and
            // whether writes are refused because none keeps up.
            let replicas = writer.replicas();
            json!({
                "role": "primary",
                "feeds_replicas": feeds,
                "encrypted": encrypted,
                "waits_for_a_replica": replicas.is_some(),
                "records": writer.held_records(),
                "replica_records": replicas.map(|(held, _)| held),
                "refuses_writes": replicas.map(|(_, is_behind)| is_behind),
            })
        }
        Some(shared) => {
            let following = lock(shared);
            json!({
                "role": "replica",
                "feeds_replicas": feeds,
                "encrypted": encrypted,
                "records": following.progress.map(|progress| progress.held),
                "primary_records": following.progress.map(|progress| progress.primary),
                "unreachable": following.unreachable,
                "stopped": following.stopped,
            })
        }
    }
}

/// A connection to a primary: as it is, or over TLS, with the proof
/// that the handshake finished.
enum Line {
    Plain(TcpStream),
    Private(Box<tls::ToPrimary>, Private),
}

impl Line {
    fn exchange<T>(
        &mut self,
        command: &[u8],
        read: impl Fn(&[u8]) -> Result<Arrived<T>, String>,
    ) -> Result<T, String> {
        match self {
            Self::Plain(stream) => exchange(stream, command, read),
            Self::Private(stream, _) => exchange(stream.as_mut(), command, read),
        }
    }
}

/// Sends `command` and reads the reply to it with `read`.
fn exchange<T>(
    stream: &mut (impl Read + Write),
    command: &[u8],
    read: impl Fn(&[u8]) -> Result<Arrived<T>, String>,
) -> Result<T, String> {
    stream
        .write_all(command)
        .and_then(|()| stream.flush())
        .map_err(|error| error.to_string())?;
    let mut input = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        if !input.is_empty() {
            match read(&input)? {
                Arrived::Whole(reply) => return Ok(reply),
                Arrived::Part(Some(whole)) if chunk.len() < whole - input.len() => {
                    chunk.resize(whole - input.len(), 0);
                }
                Arrived::Part(_) => {}
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err("the primary closed the connection".to_owned()),
            Ok(read) => input.extend_from_slice(&chunk[..read]),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The connection to a primary, made when first asked and again after
/// any ask that failed: what was on its way then is not read as the
/// answer to the next ask.
struct Wire {
    primary: String,
    /// With it the connection is TLS, the primary must show a
    /// certificate for the host followed, and keys are asked for.
    tls: Option<Arc<rustls::ClientConfig>>,
    line: Option<Line>,
}

impl Wire {
    fn connect(&self) -> io::Result<Line> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "the name has no address");
        for address in self.primary.to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, PATIENCE) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    stream.set_read_timeout(Some(PATIENCE))?;
                    stream.set_write_timeout(Some(PATIENCE))?;
                    return self.line(stream);
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn line(&self, stream: TcpStream) -> io::Result<Line> {
        let Some(tls) = &self.tls else {
            return Ok(Line::Plain(stream));
        };
        let host = self.primary.rsplit_once(':').map_or("", |(host, _)| host);
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let (stream, private) = tls::connect(tls, host, stream, PATIENCE)?;
        stream.sock.set_read_timeout(Some(PATIENCE))?;
        stream.sock.set_write_timeout(Some(PATIENCE))?;
        Ok(Line::Private(Box::new(stream), private))
    }

    /// Sends `command` on the connection, made first if there is none,
    /// and kept only if the reply came.
    fn exchange<T>(
        &mut self,
        command: &[u8],
        read: impl Fn(&[u8]) -> Result<Arrived<T>, String>,
    ) -> Result<T, String> {
        let mut line = match self.line.take() {
            Some(line) => line,
            None => self.connect().map_err(|error| error.to_string())?,
        };
        let reply = line.exchange(command, read)?;
        self.line = Some(line);
        Ok(reply)
    }
}

impl Primary for Wire {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        self.exchange(&ask_command(ask), read_answer)
    }

    /// Keys are asked for only over TLS: a connection made without it
    /// carries none, and this says so without sending anything.
    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        if self.tls.is_none() {
            return Ok(None);
        }
        let mut answers = Vec::with_capacity(wanted.len());
        for wanted in wanted.chunks(MAX_KEYS_ASKED) {
            if !matches!(self.line, None | Some(Line::Private(..))) {
                return Ok(None);
            }
            answers.extend(self.exchange(&keys_command(wanted), read_keys)?);
        }
        Ok(Some(answers))
    }
}

/// Starts the thread that follows the server at `primary`, and returns
/// what it knows of how that is going.
pub(super) fn spawn(
    writer: Writer,
    primary: String,
    tls: Option<Arc<rustls::ClientConfig>>,
) -> io::Result<Shared> {
    let shared = Shared::default();
    let told = Arc::clone(&shared);
    thread::Builder::new()
        .name("follow".to_owned())
        .spawn(move || follow(writer, primary, tls, &told))?;
    Ok(shared)
}

fn follow(
    mut writer: Writer,
    primary: String,
    tls: Option<Arc<rustls::ClientConfig>>,
    shared: &Shared,
) {
    let mut wire = Wire {
        primary,
        tls,
        line: None,
    };
    let mut held = None;
    loop {
        match follow_once(&mut wire, &mut writer, MAX_SENT) {
            Ok(progress) => {
                let brought_nothing = held.replace(progress.held) == Some(progress.held);
                let was_unreachable = {
                    let mut following = lock(shared);
                    following.progress = Some(progress);
                    following.unreachable.take().is_some()
                };
                if was_unreachable {
                    eprintln!(
                        "umbrachyx: following {} again: it holds {} records, this replica {}",
                        wire.primary, progress.primary, progress.held
                    );
                }
                if brought_nothing {
                    thread::sleep(POLL);
                }
            }
            Err(FollowError::Link(why)) => {
                let is_news = lock(shared).unreachable.replace(why.clone()).as_ref() != Some(&why);
                if is_news {
                    eprintln!(
                        "umbrachyx: no answer from {}, the server this one follows: {why}. It \
                         keeps asking, and answers reads with what it holds",
                        wire.primary
                    );
                }
                thread::sleep(RETRY);
            }
            Err(stop) => {
                let why = stop.to_string();
                eprintln!(
                    "umbrachyx: this replica no longer follows {}: {why}. It answers reads \
                     with what it holds, and is sent nothing more",
                    wire.primary
                );
                lock(shared).stopped = Some(why);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(asking: &SharedAsking) -> Seat {
        Seat {
            asking: Arc::clone(asking),
            number: None,
        }
    }

    fn held(asking: &SharedAsking) -> Vec<u64> {
        let mut held: Vec<u64> = lock_asking(asking).held.values().copied().collect();
        held.sort_unstable();
        held
    }

    fn lock_asking(asking: &SharedAsking) -> std::sync::MutexGuard<'_, Asking> {
        asking.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn a_replica_is_counted_from_its_first_answered_ask_until_its_connection_ends() {
        let asking = SharedAsking::default();
        let mut first = seat(&asking);
        let mut second = seat(&asking);
        assert_eq!(held(&asking), [] as [u64; 0]);

        first.note(3);
        second.note(7);
        first.note(5);

        assert_eq!(held(&asking), [5, 7]);
        drop(first);
        assert_eq!(held(&asking), [7]);
        drop(second);
        assert_eq!(held(&asking), [] as [u64; 0]);
    }

    #[test]
    fn a_connection_never_answered_is_not_counted_and_leaves_nothing() {
        let asking = SharedAsking::default();

        drop(seat(&asking));

        assert_eq!(held(&asking), [] as [u64; 0]);
    }

    #[test]
    fn a_replica_is_behind_an_erasure_while_it_holds_fewer_records_than_the_log_did_then() {
        let asking = SharedAsking::default();
        let mut seats = [seat(&asking), seat(&asking), seat(&asking)];
        for (seat, held) in seats.iter_mut().zip([4, 5, 9]) {
            seat.note(held);
        }

        // The erasure is the fifth record: who holds five holds it.
        assert_eq!(lock_asking(&asking).behind(Some(5)), 1);
        assert_eq!(lock_asking(&asking).behind(Some(10)), 3);
        assert_eq!(lock_asking(&asking).behind(None), 0);
    }
}
