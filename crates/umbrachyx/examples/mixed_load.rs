//! Reads and writes mixed at a fixed ratio, over the Redis protocol, to
//! any server that speaks it (`bench/mixed.sh`). Prints one CSV row.
//!
//! ```text
//! cargo run --release -p umbrachyx --example mixed_load -- \
//!     <label> <port> <clients> <seconds> <reads-percent> [key-prefix]
//! ```
//!
//! Each client is a thread with a connection of its own. It sends one
//! command, waits for the reply, and sends the next: a `GET` the given
//! share of the time and a `SET` of a 32-byte value otherwise, to a key
//! chosen at random among 1,000,000, written as `valkey-benchmark` writes
//! them (`<key-prefix><12 digits>`). The same program measures every
//! server, so no row owes anything to a difference between clients.
//!
//! The data is synthetic.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

const KEYS: u64 = 1_000_000;
const VALUE: &[u8; 32] = b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
const DEFAULT_PREFIX: &str = "key:";

type Error = Box<dyn std::error::Error + Send + Sync>;

/// How long each read and each write of one client took.
#[derive(Default)]
struct Taken {
    reads: Vec<Duration>,
    writes: Vec<Duration>,
}

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [label, port, clients, seconds, reads_percent, rest @ ..] = args.as_slice() else {
        return Err(
            "usage: mixed_load <label> <port> <clients> <seconds> <reads-percent> [key-prefix]"
                .into(),
        );
    };
    let port: u16 = port.parse()?;
    let clients: usize = clients.parse()?;
    let seconds: u64 = seconds.parse()?;
    let reads_percent: u64 = reads_percent.parse()?;
    if clients == 0 || seconds == 0 || reads_percent > 100 {
        return Err("clients and seconds are at least 1, reads-percent at most 100".into());
    }
    let prefix = rest.first().map_or(DEFAULT_PREFIX, String::as_str);

    let start = Arc::new(Barrier::new(clients + 1));
    let threads: Vec<_> = (0..clients)
        .map(|client| {
            let start = Arc::clone(&start);
            let prefix = prefix.to_owned();
            thread::spawn(move || -> Result<Taken, Error> {
                let stream = TcpStream::connect(("127.0.0.1", port));
                // Reached whether or not the connection was made, so the
                // others are not left waiting for this one.
                start.wait();
                let stream = stream?;
                stream.set_nodelay(true)?;
                run(stream, &prefix, client as u64 + 1, seconds, reads_percent)
            })
        })
        .collect();
    start.wait();
    let began = Instant::now();
    let mut all = Taken::default();
    for thread in threads {
        let taken = thread.join().map_err(|_| "a client panicked")??;
        all.reads.extend(taken.reads);
        all.writes.extend(taken.writes);
    }
    let elapsed = began.elapsed().as_secs_f64();
    let total = (all.reads.len() + all.writes.len()) as f64;
    all.reads.sort_unstable();
    all.writes.sort_unstable();
    println!(
        "\"{label}\",\"{clients}\",\"{reads_percent}\",\"{:.2}\",\"{}\",\"{}\",\"{}\",\"{}\"",
        total / elapsed,
        millis(&all.reads, 50),
        millis(&all.reads, 99),
        millis(&all.writes, 50),
        millis(&all.writes, 99),
    );
    Ok(())
}

/// One client: a command, its reply, the next, until the time is up.
fn run(
    stream: TcpStream,
    prefix: &str,
    seed: u64,
    seconds: u64,
    reads_percent: u64,
) -> Result<Taken, Error> {
    let mut replies = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    let mut random = Random(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut taken = Taken::default();
    let mut request = Vec::new();
    let end = Instant::now() + Duration::from_secs(seconds);
    loop {
        let sent = Instant::now();
        if sent >= end {
            return Ok(taken);
        }
        let key = format!("{prefix}{:012}", random.next() % KEYS);
        let is_read = random.next() % 100 < reads_percent;
        request.clear();
        if is_read {
            command(&mut request, &[b"GET", key.as_bytes()]);
        } else {
            command(&mut request, &[b"SET", key.as_bytes(), VALUE]);
        }
        stream.write_all(&request)?;
        read_reply(&mut replies)?;
        let took = sent.elapsed();
        if is_read {
            taken.reads.push(took);
        } else {
            taken.writes.push(took);
        }
    }
}

fn command(out: &mut Vec<u8>, args: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

/// Reads one reply to a `GET` or a `SET`. An error reply ends the run: a
/// row must not count commands the server refused.
fn read_reply(replies: &mut impl BufRead) -> Result<(), Error> {
    let mut line = String::new();
    replies.read_line(&mut line)?;
    let line = line.trim_end();
    match line.as_bytes().first() {
        Some(b'+') => Ok(()),
        Some(b'$') => {
            let len: i64 = line[1..].parse()?;
            if let Ok(len) = usize::try_from(len) {
                // The value and the line end after it.
                let mut body = vec![0; len + 2];
                replies.read_exact(&mut body)?;
            }
            Ok(())
        }
        _ => Err(format!("the server answered {line:?}").into()),
    }
}

/// The time at `percent` of the way through sorted times, in
/// milliseconds; empty when there is none, as for writes at 100 % reads.
fn millis(sorted: &[Duration], percent: usize) -> String {
    if sorted.is_empty() {
        return String::new();
    }
    let at = (sorted.len() * percent / 100).min(sorted.len() - 1);
    format!("{:.3}", sorted[at].as_secs_f64() * 1_000.0)
}

/// Xorshift: enough to spread keys and commands, and the same on every
/// run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}
