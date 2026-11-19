//! Replication between servers: one that hands its log to replicas, and
//! one that follows another and answers reads.
//!
//! A replica is a client of its primary, on a port kept for replicas. It
//! sends one command, `UMBRA.LOG.AFTER <log> <records> <chain> <max
//! bytes>`, which says what it holds, and is answered with the records
//! after those, or with why it is sent none (see `store::follow`).
//! Nothing is encrypted and no token is asked for, as for reads: the
//! records hold every branch, with values still sealed. So a server
//! listens for replicas only when whoever started it named the port,
//! which only its replicas should reach.
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
//! goes on answering reads. `UMBRA.REPLICATION` says which of these it
//! is, and how far behind, to any client; it does not say where the
//! primary is, which is whoever started the server's to know.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use serde_json::json;

use super::resp::{self, Reply};
use crate::log::{Chain, LogId, MAX_PAYLOAD_LEN, record};
use crate::store::follow::{
    Ask, FollowError, Held, MAX_SENT, Primary, Progress, Refusal, Sent, answer, follow_once,
};
use crate::store::{StoreError, Writer};

const LOG_AFTER: &str = "UMBRA.LOG.AFTER";
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
/// `after` and `records`, in front of the frames of an answer.
const ANSWER_HEAD: usize = 16;
/// The longest answer: a record is never cut, so one larger than a
/// primary otherwise sends is an answer on its own, and a record is at
/// most this long.
const MAX_ANSWER: usize = ANSWER_HEAD + record::HEADER_LEN + MAX_PAYLOAD_LEN;
const _: () = assert!(MAX_SENT <= record::HEADER_LEN + MAX_PAYLOAD_LEN);
/// How long a replica's connection may say nothing before the primary
/// closes it. A replica asks every millisecond, and one that went
/// without closing would keep its place among the few answered.
const QUIET: Duration = Duration::from_secs(60);
/// Replicas answered at once. One more is told so and closed.
const MAX_REPLICAS: usize = 16;
const ONLY_REPLICAS: &str = "ERR this port answers replicas only: UMBRA.LOG.AFTER";

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

/// The reply to a replication command a client may send, or `None` if
/// `name` is none.
pub(super) fn reply(
    feeds: bool,
    following: Option<&Shared>,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    match name {
        REPLICATION if args.is_empty() => Some(status(feeds, following)),
        REPLICATION => Some(Reply::Error(format!(
            "ERR wrong number of arguments for '{}' command",
            REPLICATION.to_ascii_lowercase()
        ))),
        _ => None,
    }
}

/// Starts the thread that takes replicas' connections on `listener`,
/// and gives each a thread that answers it.
pub(super) fn spawn_feed(writer: Writer, listener: Arc<TcpListener>) -> io::Result<()> {
    let replicas = Arc::new(AtomicUsize::new(0));
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
                let started = thread::Builder::new()
                    .name("feed a replica".to_owned())
                    .spawn(move || match slot {
                        Some(_slot) => feed(stream, writer),
                        None => super::refuse(stream),
                    });
                if let Err(error) = started {
                    eprintln!("umbrachyx: a replica's thread could not start: {error}");
                }
            }
        })
        .map(drop)
}

/// Answers one replica until it goes, or sends what is no command.
fn feed(mut stream: TcpStream, mut writer: Writer) {
    // An answer goes out when it is ready, not when more follows.
    let _ = stream.set_nodelay(true);
    if stream.set_read_timeout(Some(QUIET)).is_err() {
        return;
    }
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
        feed_reply(&mut writer, &command).write_to(&mut out);
        if stream.write_all(&out).is_err() {
            return;
        }
    }
}

fn feed_reply(writer: &mut Writer, command: &[Vec<u8>]) -> Reply {
    let Some((name, args)) = command.split_first() else {
        return Reply::Error(ONLY_REPLICAS.to_owned());
    };
    if !name.eq_ignore_ascii_case(LOG_AFTER.as_bytes()) {
        return Reply::Error(ONLY_REPLICAS.to_owned());
    }
    let ask = match read_ask(args) {
        Ok(ask) => ask,
        Err(error) => return Reply::Error(format!("ERR {error}")),
    };
    match held_answer(writer, &ask) {
        Ok(answer) => answer_reply(answer),
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

fn status(feeds: bool, following: Option<&Shared>) -> Reply {
    let status = match following {
        None => json!({ "role": "primary", "feeds_replicas": feeds }),
        Some(shared) => {
            let following = lock(shared);
            json!({
                "role": "replica",
                "feeds_replicas": feeds,
                "records": following.progress.map(|progress| progress.held),
                "primary_records": following.progress.map(|progress| progress.primary),
                "unreachable": following.unreachable,
                "stopped": following.stopped,
            })
        }
    };
    Reply::Bulk(Some(status.to_string().into_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != 2 * N {
        return None;
    }
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(text.chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

fn number<T: std::str::FromStr>(text: &[u8]) -> Option<T> {
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// The command that asks.
fn ask_command(ask: &Ask) -> Vec<u8> {
    let args = [
        LOG_AFTER.to_owned(),
        hex(&ask.held.log),
        ask.held.records.to_string(),
        hex(&ask.held.chain),
        ask.max_bytes.to_string(),
    ];
    let mut command = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        command.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    command
}

fn read_ask(args: &[Vec<u8>]) -> Result<Ask, String> {
    let [log, records, chain, max_bytes] = args else {
        return Err(format!(
            "wrong number of arguments for '{}' command",
            LOG_AFTER.to_ascii_lowercase()
        ));
    };
    let log: LogId = unhex(log).ok_or("the log's name is 32 hex digits")?;
    let chain: Chain = unhex(chain).ok_or("the chain is 64 hex digits")?;
    let records = number(records).ok_or("the number of records held is not a number")?;
    let max_bytes = number(max_bytes).ok_or("the most bytes to send is not a number")?;
    Ok(Ask {
        held: Held {
            log,
            records,
            chain,
        },
        max_bytes,
    })
}

/// A refusal is an error whose first word names it; what is sent is one
/// value: where the frames start and how many records the primary
/// holds, eight bytes each, then the frames.
fn answer_reply(answer: Result<Sent, Refusal>) -> Reply {
    match answer {
        Ok(sent) => {
            let mut value = Vec::with_capacity(ANSWER_HEAD + sent.frames.len());
            value.extend_from_slice(&sent.after.to_le_bytes());
            value.extend_from_slice(&sent.records.to_le_bytes());
            value.extend_from_slice(&sent.frames);
            Reply::Bulk(Some(value))
        }
        Err(refusal) => {
            let word = match refusal {
                Refusal::AnotherLog => "ANOTHERLOG".to_owned(),
                Refusal::Fewer { records } => format!("FEWER {records}"),
                Refusal::Diverged => "DIVERGED".to_owned(),
            };
            Reply::Error(format!("{word} {refusal}"))
        }
    }
}

/// What the start of `input` holds of a reply to an ask.
enum Arrived {
    /// Not all of it yet. With this many bytes in all it would be whole,
    /// if that can be told already.
    Part(Option<usize>),
    Answer(Result<Sent, Refusal>),
}

/// Reads the reply to an ask from the start of `input`. An error is a
/// reply that is no answer: the server said why, or it is not one of
/// ours.
fn read_answer(input: &[u8]) -> Result<Arrived, String> {
    let Some(line_end) = input.windows(2).position(|pair| pair == b"\r\n") else {
        return Ok(Arrived::Part(None));
    };
    let line = &input[1..line_end];
    match input[0] {
        b'-' => {
            let text = String::from_utf8_lossy(line);
            let mut words = text.split(' ');
            match (words.next(), words.next()) {
                (Some("ANOTHERLOG"), _) => Ok(Arrived::Answer(Err(Refusal::AnotherLog))),
                (Some("DIVERGED"), _) => Ok(Arrived::Answer(Err(Refusal::Diverged))),
                (Some("FEWER"), Some(records)) => match records.parse() {
                    Ok(records) => Ok(Arrived::Answer(Err(Refusal::Fewer { records }))),
                    Err(_) => Err(text.into_owned()),
                },
                _ => Err(text.into_owned()),
            }
        }
        b'$' => {
            let len: usize = number(line)
                .filter(|&len| (ANSWER_HEAD..=MAX_ANSWER).contains(&len))
                .ok_or("the answer is not one a primary sends")?;
            let start = line_end + 2;
            // With the line end after it, or the next answer on this
            // connection would start with one.
            let Some(value) = input.get(start..start + len + 2) else {
                return Ok(Arrived::Part(Some(start + len + 2)));
            };
            let value = &value[..len];
            let word = |at: usize| {
                let mut bytes = [0; 8];
                bytes.copy_from_slice(&value[at..at + 8]);
                u64::from_le_bytes(bytes)
            };
            Ok(Arrived::Answer(Ok(Sent {
                after: word(0),
                records: word(8),
                frames: value[ANSWER_HEAD..].to_vec(),
            })))
        }
        _ => Err("the answer is not one a primary sends".to_owned()),
    }
}

/// The connection to a primary, made when first asked and again after
/// any ask that failed: what was on its way then is not read as the
/// answer to the next ask.
struct Wire {
    primary: String,
    stream: Option<TcpStream>,
}

impl Wire {
    fn connect(&self) -> io::Result<TcpStream> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "the name has no address");
        for address in self.primary.to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, PATIENCE) {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    stream.set_read_timeout(Some(PATIENCE))?;
                    stream.set_write_timeout(Some(PATIENCE))?;
                    return Ok(stream);
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn exchange(stream: &mut TcpStream, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        stream
            .write_all(&ask_command(ask))
            .map_err(|error| error.to_string())?;
        let mut input = Vec::new();
        let mut chunk = vec![0; 64 * 1024];
        loop {
            if !input.is_empty() {
                match read_answer(&input)? {
                    Arrived::Answer(answer) => return Ok(answer),
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
}

impl Primary for Wire {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        let mut stream = match self.stream.take() {
            Some(stream) => stream,
            None => self.connect().map_err(|error| error.to_string())?,
        };
        let answer = Self::exchange(&mut stream, ask)?;
        self.stream = Some(stream);
        Ok(answer)
    }
}

/// Starts the thread that follows the server at `primary`, and returns
/// what it knows of how that is going.
pub(super) fn spawn(writer: Writer, primary: String) -> io::Result<Shared> {
    let shared = Shared::default();
    let told = Arc::clone(&shared);
    thread::Builder::new()
        .name("follow".to_owned())
        .spawn(move || follow(writer, primary, &told))?;
    Ok(shared)
}

fn follow(mut writer: Writer, primary: String, shared: &Shared) {
    let mut wire = Wire {
        primary,
        stream: None,
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

    fn ask() -> Ask {
        Ask {
            held: Held {
                log: *b"a log's own name",
                records: 12_345,
                chain: [0xa7; 32],
            },
            max_bytes: 4096,
        }
    }

    fn sent() -> Sent {
        Sent {
            after: 12_345,
            records: 12_400,
            frames: b"frames, with \r\n in them".to_vec(),
        }
    }

    fn wire(answer: Result<Sent, Refusal>) -> Vec<u8> {
        let mut out = Vec::new();
        answer_reply(answer).write_to(&mut out);
        out
    }

    fn arrived(input: &[u8]) -> Option<Result<Sent, Refusal>> {
        match read_answer(input).expect("is an answer") {
            Arrived::Answer(answer) => Some(answer),
            Arrived::Part(_) => None,
        }
    }

    #[test]
    fn an_ask_is_read_as_it_was_written() {
        let command = ask_command(&ask());

        let (parsed, len) = super::super::resp::parse(&command)
            .expect("is a command")
            .expect("is whole");

        assert_eq!(len, command.len());
        assert_eq!(parsed[0], LOG_AFTER.as_bytes());
        assert_eq!(read_ask(&parsed[1..]), Ok(ask()));
    }

    #[test]
    fn an_ask_that_is_not_one_is_refused_with_what_is_wrong() {
        let mut args: Vec<Vec<u8>> = super::super::resp::parse(&ask_command(&ask()))
            .expect("is a command")
            .expect("is whole")
            .0[1..]
            .to_vec();
        args[0].pop();

        assert_eq!(
            read_ask(&args),
            Err("the log's name is 32 hex digits".to_owned())
        );
        assert!(read_ask(&args[1..]).is_err());
    }

    #[test]
    fn every_answer_is_read_as_it_was_written() {
        for answer in [
            Ok(sent()),
            Ok(Sent {
                frames: Vec::new(),
                ..sent()
            }),
            Err(Refusal::AnotherLog),
            Err(Refusal::Fewer { records: 7 }),
            Err(Refusal::Diverged),
        ] {
            assert_eq!(arrived(&wire(answer.clone())), Some(answer));
        }
    }

    #[test]
    fn an_answer_cut_short_anywhere_is_waited_for() {
        let whole = wire(Ok(sent()));

        for len in 1..whole.len() {
            assert_eq!(arrived(&whole[..len]), None, "{len} bytes");
        }
    }

    #[test]
    fn a_reply_that_is_no_answer_is_said_as_it_came() {
        let mut out = Vec::new();
        Reply::Error(ONLY_REPLICAS.to_owned()).write_to(&mut out);

        let said = read_answer(&out).err();

        assert_eq!(said.as_deref(), Some(ONLY_REPLICAS));
        assert!(read_answer(b"+OK\r\n").is_err());
        assert!(read_answer(b"$3\r\nabc\r\n").is_err());
    }
}
