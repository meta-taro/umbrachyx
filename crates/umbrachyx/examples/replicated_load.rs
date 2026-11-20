//! Load for a primary and its replicas, over the Redis protocol, to any
//! servers that speak it (`bench/replicated.sh`). The same program
//! measures every product, so no row owes anything to a difference
//! between clients. Prints one CSV row.
//!
//! ```text
//! replicated_load load <label> <seconds> <primary> <writers> <wait> <readers> [replica]...
//! replicated_load lag  <label> <writes>  <primary> <wait> <replica>
//! ```
//!
//! Servers are named `host:port`. `load` writes to the primary from
//! `<writers>` clients and reads from the replicas from `<readers>`
//! clients, spread over them in turn, for `<seconds>`. Each client is a
//! thread with a connection of its own that sends one command, waits for
//! the reply and sends the next: a `SET` of a 32-byte value or a `GET`,
//! of a key chosen at random among 1,000,000, written as
//! `valkey-benchmark` writes them.
//!
//! `<wait>` is what a writer does after each `SET` before it counts the
//! write as answered: `none`, or `waitaof`, which sends `WAITAOF 1 1 0`
//! and reads its reply, so the write is on the primary's disk and one
//! replica's. A server started to answer only once a replica holds a
//! write needs `none`: its `SET` already waited.
//!
//! `lag` is one client that writes a value to the primary and then asks
//! the replica for it until it has it, `<writes>` times. What it prints
//! is the time from the write's answer to the read that found it; a read
//! takes one round trip, which is the least it can show.
//!
//! The data is synthetic.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const KEYS: u64 = 1_000_000;
const VALUE_LEN: usize = 32;
const PREFIX: &str = "key:";

type Error = Box<dyn std::error::Error + Send + Sync>;

/// What a writer does after a `SET` before the write counts as answered.
#[derive(Clone, Copy, PartialEq)]
enum Wait {
    Nothing,
    /// `WAITAOF 1 1 0`: synced on the primary and on one replica.
    Aof,
}

impl Wait {
    fn read(word: &str) -> Result<Self, Error> {
        match word {
            "none" => Ok(Self::Nothing),
            "waitaof" => Ok(Self::Aof),
            other => Err(format!("wait is none or waitaof, not {other}").into()),
        }
    }
}

/// A connection to one server.
struct Client {
    stream: TcpStream,
    replies: BufReader<TcpStream>,
    request: Vec<u8>,
}

impl Client {
    fn connect(server: &str) -> Result<Self, Error> {
        let stream = TcpStream::connect(server)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            replies: BufReader::new(stream.try_clone()?),
            stream,
            request: Vec::new(),
        })
    }

    /// Sends one command and returns the value of its reply, if it is
    /// one. An error reply ends the run: a row must not count commands
    /// a server refused.
    fn ask(&mut self, args: &[&[u8]]) -> Result<Option<Vec<u8>>, Error> {
        self.request.clear();
        self.request
            .extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
        for arg in args {
            self.request
                .extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            self.request.extend_from_slice(arg);
            self.request.extend_from_slice(b"\r\n");
        }
        self.stream.write_all(&self.request)?;
        self.reply()
    }

    fn reply(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let mut line = String::new();
        self.replies.read_line(&mut line)?;
        let line = line.trim_end();
        match line.as_bytes().first() {
            Some(b'+' | b':') => Ok(None),
            Some(b'$') => {
                let Ok(len) = usize::try_from(line[1..].parse::<i64>()?) else {
                    return Ok(None);
                };
                // The value and the line end after it.
                let mut body = vec![0; len + 2];
                self.replies.read_exact(&mut body)?;
                body.truncate(len);
                Ok(Some(body))
            }
            Some(b'*') => {
                for _ in 0..line[1..].parse::<usize>()? {
                    self.reply()?;
                }
                Ok(None)
            }
            _ => Err(format!("a server answered {line:?}").into()),
        }
    }

    fn set(&mut self, key: &[u8], value: &[u8], wait: Wait) -> Result<(), Error> {
        self.ask(&[b"SET", key, value])?;
        if wait == Wait::Aof {
            self.ask(&[b"WAITAOF", b"1", b"1", b"0"])?;
        }
        Ok(())
    }
}

/// Xorshift: enough to spread keys, and the same on every run.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn key(&mut self) -> String {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        format!("{PREFIX}{:012}", self.0 % KEYS)
    }
}

/// The time at `percent` of the way through sorted times, in
/// milliseconds; empty when there is none.
fn millis(sorted: &[Duration], percent: usize) -> String {
    if sorted.is_empty() {
        return String::new();
    }
    let at = (sorted.len() * percent / 100).min(sorted.len() - 1);
    format!("{:.3}", sorted[at].as_secs_f64() * 1_000.0)
}

/// One client's commands until the time is up, and how long each took.
fn run(
    server: &str,
    start: &Barrier,
    seed: u64,
    seconds: u64,
    writes: Option<Wait>,
) -> Result<Vec<Duration>, Error> {
    let client = Client::connect(server);
    // Reached whether or not the connection was made, so the others are
    // not left waiting for this one.
    start.wait();
    let mut client = client?;
    let mut random = Random::new(seed);
    let mut taken = Vec::new();
    let end = Instant::now() + Duration::from_secs(seconds);
    loop {
        let sent = Instant::now();
        if sent >= end {
            return Ok(taken);
        }
        let key = random.key();
        match writes {
            Some(wait) => client.set(key.as_bytes(), &[b'x'; VALUE_LEN], wait)?,
            None => drop(client.ask(&[b"GET", key.as_bytes()])?),
        }
        taken.push(sent.elapsed());
    }
}

fn load(args: &[String]) -> Result<(), Error> {
    let [
        label,
        seconds,
        primary,
        writers,
        wait,
        readers,
        replicas @ ..,
    ] = args
    else {
        return Err(
            "load <label> <seconds> <primary> <writers> <wait> <readers> [replica]...".into(),
        );
    };
    let seconds: u64 = seconds.parse()?;
    let (writers, readers): (usize, usize) = (writers.parse()?, readers.parse()?);
    let wait = Wait::read(wait)?;
    if seconds == 0 || writers + readers == 0 || (readers > 0 && replicas.is_empty()) {
        return Err("at least a second, a client, and a replica to read from".into());
    }
    let start = Arc::new(Barrier::new(writers + readers + 1));
    let spawn = |server: &str, seed: usize, writes: Option<Wait>| {
        let (server, start) = (server.to_owned(), Arc::clone(&start));
        thread::spawn(move || run(&server, &start, seed as u64 + 1, seconds, writes))
    };
    let writing: Vec<_> = (0..writers)
        .map(|n| spawn(primary, n, Some(wait)))
        .collect();
    let reading: Vec<_> = (0..readers)
        .map(|n| spawn(&replicas[n % replicas.len()], writers + n, None))
        .collect();
    start.wait();
    let began = Instant::now();
    let gather = |threads: Vec<thread::JoinHandle<Result<Vec<Duration>, Error>>>| {
        let mut all = Vec::new();
        for thread in threads {
            all.extend(thread.join().map_err(|_| "a client panicked")??);
        }
        all.sort_unstable();
        Ok::<_, Error>(all)
    };
    let (writes, reads) = (gather(writing)?, gather(reading)?);
    let elapsed = began.elapsed().as_secs_f64();
    println!(
        "\"{label}\",\"{writers}\",\"{readers}\",\"{}\",\"{:.2}\",\"{}\",\"{}\",\"{:.2}\",\"{}\",\"{}\"",
        replicas.len(),
        writes.len() as f64 / elapsed,
        millis(&writes, 50),
        millis(&writes, 99),
        reads.len() as f64 / elapsed,
        millis(&reads, 50),
        millis(&reads, 99),
    );
    Ok(())
}

fn lag(args: &[String]) -> Result<(), Error> {
    let [label, writes, primary, wait, replica] = args else {
        return Err("lag <label> <writes> <primary> <wait> <replica>".into());
    };
    let writes: usize = writes.parse()?;
    let wait = Wait::read(wait)?;
    let (mut primary, mut replica) = (Client::connect(primary)?, Client::connect(replica)?);
    let mut random = Random::new(1);
    let mut lags = Vec::with_capacity(writes);
    for write in 0..writes {
        let key = random.key();
        // A value no earlier write to the key had.
        let value = format!("{write:0VALUE_LEN$}");
        primary.set(key.as_bytes(), value.as_bytes(), wait)?;
        let answered = Instant::now();
        while replica.ask(&[b"GET", key.as_bytes()])?.as_deref() != Some(value.as_bytes()) {}
        lags.push(answered.elapsed());
    }
    lags.sort_unstable();
    println!(
        "\"{label}\",\"{writes}\",\"{}\",\"{}\",\"{}\"",
        millis(&lags, 50),
        millis(&lags, 99),
        millis(&lags, 100),
    );
    Ok(())
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((command, rest)) if command == "load" => load(rest),
        Some((command, rest)) if command == "lag" => lag(rest),
        _ => Err("load or lag; see the top of the file".into()),
    }
}
