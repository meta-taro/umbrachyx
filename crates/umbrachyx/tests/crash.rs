//! Kill a writing process at random moments and check that every write it
//! acknowledged is still in the log afterwards.
//!
//! The test binary re-runs itself as the writer: with `UMBRACHYX_CRASH_WRITER`
//! set, the test body appends records forever and prints the sequence number
//! of each one only after `append` has returned. The parent kills it with
//! SIGKILL (TerminateProcess on Windows) after a pseudo-random delay, reopens the log and compares.
//!
//! What this proves and what it does not: a killed process loses nothing the
//! kernel already holds, so this checks that acknowledgement waits for the
//! write and that recovery handles a write cut mid-frame. It does not
//! simulate a power cut; that is the job of the deterministic simulation disk.
//!
//! `cargo test --test crash -- --ignored` runs the full 1,000 rounds;
//! `UMBRACHYX_CRASH_ROUNDS` overrides the count.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use umbrachyx::log::{Edit, FileStorage, Log, Record};

const WRITER_ENV: &str = "UMBRACHYX_CRASH_WRITER";
const ROUNDS_ENV: &str = "UMBRACHYX_CRASH_ROUNDS";
const QUICK_ROUNDS: u32 = 20;
const FULL_ROUNDS: u32 = 1_000;
const MAX_KILL_DELAY_MICROS: u64 = 20_000;
const SEED: u64 = 0x5EED_0001;
/// Printed once the writer has opened the log. The kill delay starts here,
/// so slow process start-up (Windows) does not eat the delay before any
/// write happens.
const READY: &str = "ready";
const READY_TIMEOUT: Duration = Duration::from_secs(30);

fn record(seq: u64) -> Record {
    Record {
        branch: "main".to_owned(),
        changes: vec![Edit {
            key: format!("player:{}:coins", seq % 97).into_bytes(),
            value: Some(seq.to_le_bytes().to_vec()),
        }],
        who: "agent:crash-test".to_owned(),
        why: format!("write {seq}"),
        approved_by: seq.is_multiple_of(3).then(|| "ops:reviewer".to_owned()),
        time: seq,
    }
}

/// The child side: append until killed, announcing each acknowledged write.
fn run_writer(path: &Path) -> ! {
    let storage = FileStorage::open(path).expect("writer opens storage");
    let (mut log, recovered) = Log::open(storage).expect("writer opens log");
    let mut out = std::io::stdout().lock();
    writeln!(out, "{READY}").expect("writer reports");
    out.flush().expect("writer flushes");
    let mut seq = recovered.len() as u64;
    loop {
        log.append(&record(seq)).expect("writer appends");
        writeln!(out, "{seq}").expect("writer reports");
        out.flush().expect("writer flushes");
        seq += 1;
    }
}

/// SplitMix64, so the kill delays repeat for a given seed with no dependency.
fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn spawn_writer(path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--exact", "kill_during_writes", "--ignored", "--nocapture"])
        .env(WRITER_ENV, path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawns writer")
}

/// Kill the writer `delay` after it is ready and return the highest
/// sequence it acknowledged.
fn kill_after(mut child: Child, delay: Duration) -> Option<u64> {
    let stdout = child.stdout.take().expect("writer stdout");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let line = line.trim();
            // The receivers outlive the reader; a send error cannot happen.
            if line == READY {
                let _ = ready_tx.send(());
            } else if let Ok(seq) = line.parse::<u64>() {
                let _ = tx.send(seq);
            }
        }
    });
    let is_ready = ready_rx.recv_timeout(READY_TIMEOUT).is_ok();
    if is_ready {
        thread::sleep(delay);
    }
    child.kill().expect("kills writer");
    child.wait().expect("reaps writer");
    reader.join().expect("reader thread");
    assert!(
        is_ready,
        "writer did not open the log within {READY_TIMEOUT:?}"
    );
    rx.try_iter().max()
}

fn rounds() -> u32 {
    std::env::var(ROUNDS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FULL_ROUNDS)
}

struct TempLog(PathBuf);

impl Drop for TempLog {
    fn drop(&mut self) {
        // Best effort: a leftover file in the temp dir harms nothing.
        let _ = std::fs::remove_file(&self.0);
    }
}

fn crash_rounds(name: &str, rounds: u32) {
    let path = TempLog(
        std::env::temp_dir().join(format!("umbrachyx-crash-{name}-{}.log", std::process::id())),
    );
    let _ = std::fs::remove_file(&path.0);
    let mut random = SEED;
    let mut acknowledged = 0u64;

    for round in 0..rounds {
        let delay = Duration::from_micros(next_random(&mut random) % MAX_KILL_DELAY_MICROS);
        if let Some(last) = kill_after(spawn_writer(&path.0), delay) {
            acknowledged = acknowledged.max(last + 1);
        }

        let storage = FileStorage::open(&path.0).expect("opens storage");
        let (_, recovered) = Log::open(storage)
            .unwrap_or_else(|e| panic!("round {round} (seed {SEED:#x}): log fails to open: {e}"));

        assert!(
            recovered.len() as u64 >= acknowledged,
            "round {round} (seed {SEED:#x}): {} records recovered, {acknowledged} acknowledged",
            recovered.len()
        );
        for (seq, found) in recovered.iter().enumerate() {
            assert_eq!(
                found,
                &record(seq as u64),
                "round {round}: record {seq} differs"
            );
        }
    }
    assert!(
        acknowledged > 0,
        "no write was ever acknowledged; the test proved nothing"
    );
    println!("{rounds} rounds, {acknowledged} acknowledged writes, none lost");
}

#[test]
fn quick_kill_during_writes() {
    crash_rounds("quick", QUICK_ROUNDS);
}

#[test]
#[ignore = "1,000 rounds; run with --ignored"]
fn kill_during_writes() {
    if let Some(path) = std::env::var_os(WRITER_ENV) {
        run_writer(Path::new(&path));
    }
    crash_rounds("full", rounds());
}
