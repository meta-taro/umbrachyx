//! Kill the server while clients write to it over the Redis protocol, and
//! check that every SET it acknowledged is there after it restarts.
//!
//! Each round starts `umbrachyx serve`, lets clients write, kills the server
//! with SIGKILL (TerminateProcess on Windows) at a pseudo-random moment, and
//! reopens the log. Half the clients send one SET at a time; the other half
//! pipeline several, so writes that share a sync are cut too. A client keeps
//! a write only once it has read the `+OK` for it.
//!
//! Like the log's crash test, this does not simulate a power cut: a killed
//! process loses nothing the kernel already holds. It checks that the server
//! answers only after the write is durable, end to end.
//!
//! `cargo test --release --test server_crash -- --ignored` runs the full 100
//! rounds; `UMBRACHYX_SERVER_CRASH_ROUNDS` overrides the count. Run it in
//! release mode: the log grows every round and the server replays all of it
//! when it starts, which in a debug build makes the full run take hours.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use umbrachyx::log::FileStorage;
use umbrachyx::store::{MAIN_BRANCH, Store, SystemClock};
use umbrachyx::tree::Bytes;

const BRANCH: &str = "bench";
const ROUNDS_ENV: &str = "UMBRACHYX_SERVER_CRASH_ROUNDS";
const QUICK_ROUNDS: u32 = 3;
const FULL_ROUNDS: u32 = 100;
const CLIENTS: usize = 16;
/// SETs a pipelining client sends before reading their replies.
const PIPELINE: usize = 16;
const MIN_KILL_DELAY_MILLIS: u64 = 20;
const MAX_KILL_DELAY_MILLIS: u64 = 300;
const SEED: u64 = 0x5EED_0002;
/// The log grows every round and the server replays all of it before it
/// listens, which by the last rounds takes seconds on a slow CI runner.
const START_TIMEOUT: Duration = Duration::from_secs(120);
const START_WAIT: Duration = Duration::from_millis(25);

/// SplitMix64, so the kill delays repeat for a given seed with no dependency.
fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A fresh directory per run, removed when it ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-crash-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self) -> PathBuf {
        self.0.join("log")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover directory in the temp dir harms nothing.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn umbrachyx(log: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_umbrachyx"));
    command
        .args(args)
        .env("UMBRACHYX_DB", log)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    listener.local_addr().expect("has an address").port()
}

/// Kills the server when dropped, so a failed round leaves no process.
struct Server(Child);

impl Server {
    fn kill(mut self) {
        self.0.kill().expect("kills the server");
        self.0.wait().expect("reaps the server");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Already reaped after `kill`; otherwise best effort while failing.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the server, its error output going to `errors`, and waits until
/// it listens. A server that exits or never listens fails the round with
/// what it wrote.
fn start_server(log: &Path, errors: &Path) -> (Server, u16) {
    let port = free_port();
    let error_file = File::create(errors).expect("creates the error file");
    let mut server = Server(
        umbrachyx(log, &["serve", BRANCH, "--port", &port.to_string()])
            .stderr(error_file)
            .spawn()
            .expect("starts the server"),
    );
    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (server, port);
        }
        if let Some(status) = server.0.try_wait().expect("checks the server") {
            panic!(
                "the server exited with {status}: {}",
                std::fs::read_to_string(errors).unwrap_or_default()
            );
        }
        thread::sleep(START_WAIT);
    }
    panic!(
        "the server did not listen on {port} within {:?}: {}",
        started.elapsed(),
        std::fs::read_to_string(errors).unwrap_or_default()
    );
}

fn set(key: &str, value: &str) -> Vec<u8> {
    format!(
        "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{value}\r\n",
        key.len(),
        value.len()
    )
    .into_bytes()
}

/// Writes until the server goes away; returns the writes it acknowledged.
fn client(port: u16, round: u32, client: usize) -> Vec<(String, String)> {
    let mut acknowledged = Vec::new();
    let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return acknowledged;
    };
    let mut writer = stream.try_clone().expect("clones the stream");
    let mut replies = BufReader::new(stream);
    let depth = if client.is_multiple_of(2) {
        1
    } else {
        PIPELINE
    };
    let mut line = String::new();
    for batch in 0.. {
        let writes: Vec<_> = (0..depth)
            .map(|n| {
                let seq = batch * depth + n;
                (
                    format!("round/{round}/client/{client}/{seq}"),
                    format!("{round}-{client}-{seq}"),
                )
            })
            .collect();
        let request: Vec<u8> = writes.iter().flat_map(|(k, v)| set(k, v)).collect();
        if writer.write_all(&request).is_err() {
            return acknowledged;
        }
        for write in writes {
            line.clear();
            match replies.read_line(&mut line) {
                Ok(_) if line == "+OK\r\n" => acknowledged.push(write),
                // Killed mid-reply, or never answered: not acknowledged.
                _ => return acknowledged,
            }
        }
    }
    acknowledged
}

fn rounds() -> u32 {
    std::env::var(ROUNDS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FULL_ROUNDS)
}

fn kill_rounds(name: &str, rounds: u32) {
    let dir = TempDir::new(name);
    let created = umbrachyx(&dir.log(), &["branch", BRANCH, "--from", MAIN_BRANCH])
        .status()
        .expect("runs");
    assert!(created.success(), "creates the branch");
    let mut random = SEED;
    let mut acknowledged = Vec::new();

    for round in 0..rounds {
        let span = MAX_KILL_DELAY_MILLIS - MIN_KILL_DELAY_MILLIS;
        let delay = MIN_KILL_DELAY_MILLIS + next_random(&mut random) % span;
        let (server, port) = start_server(&dir.log(), &dir.0.join("serve.err"));
        let clients: Vec<_> = (0..CLIENTS)
            .map(|n| thread::spawn(move || client(port, round, n)))
            .collect();
        thread::sleep(Duration::from_millis(delay));
        server.kill();
        for handle in clients {
            acknowledged.extend(handle.join().expect("client finishes"));
        }

        let storage = FileStorage::open(&dir.log()).expect("opens storage");
        let store = Store::open(storage, SystemClock).unwrap_or_else(|e| {
            panic!("round {round} (seed {SEED:#x}): the store fails to open: {e}")
        });
        for (key, value) in &acknowledged {
            assert_eq!(
                store.get(BRANCH, key.as_bytes()),
                Some(Bytes::from(value.as_bytes())),
                "round {round} (seed {SEED:#x}): acknowledged {key} is missing or wrong"
            );
        }
    }
    assert!(
        !acknowledged.is_empty(),
        "no write was ever acknowledged; the test proved nothing"
    );
    println!(
        "{rounds} rounds, {} acknowledged SETs, none lost",
        acknowledged.len()
    );
}

#[test]
fn quick_kill_the_server_during_writes() {
    kill_rounds("quick", QUICK_ROUNDS);
}

#[test]
#[ignore = "100 rounds; run with --ignored"]
fn kill_the_server_during_writes() {
    kill_rounds("full", rounds());
}
