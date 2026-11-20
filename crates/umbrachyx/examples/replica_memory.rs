//! How long a replica takes to read its log when it starts, and what a
//! read of a current value costs it, when it keeps the history and when
//! it keeps current values only (`serve --follow --current-values-only`).
//! What a replica holds in memory while it follows is measured on two
//! servers, by `bench/replica_memory.sh`.
//!
//! ```sh
//! cargo run --release --example replica_memory -- make <keys> <passes> <log>
//! cargo run --release --example replica_memory -- history <keys> <log>
//! cargo run --release --example replica_memory -- current <keys> <log>
//! ```
//!
//! `make` writes a log as a primary would: `<passes>` passes of `<keys>`
//! commits of one key each, 32-byte values over a space of as many keys,
//! in groups of 64 as a pipelining client sends them. The other two open
//! that log as a replica does when it starts, each in a process of its
//! own, and print how long opening took, the resident set afterwards
//! (read with `ps`, so this runs on macOS and Linux) and what a read of
//! a current value costs. The resident set here is mostly what reading
//! the log took, the file and its records at once, and says little of
//! what is held afterwards. Record the machine alongside the output.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{CommitRequest, MAIN_BRANCH, Provenance, Store, SystemClock};

const GROUP: usize = 64;
const VALUE_LEN: usize = 32;
const READS: usize = 2_000_000;

type FileStore = Store<FileStorage, SystemClock>;

/// A small deterministic generator, so every pass writes the same keys.
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Self(0x9E37_79B9_7F4A_7C15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn key(n: u64) -> Vec<u8> {
    format!("player/{n}").into_bytes()
}

fn make(keys: usize, passes: usize, log: &Path) {
    let storage = FileStorage::open(log).expect("opens log");
    let mut store: FileStore = Store::open(storage, SystemClock).expect("opens store");
    for _ in 0..passes {
        let mut rng = Rng::new();
        let mut written = 0;
        while written < keys {
            let requests = (0..GROUP.min(keys - written))
                .map(|_| CommitRequest {
                    branch: MAIN_BRANCH.to_owned(),
                    edits: vec![Edit {
                        key: key(rng.next() % keys as u64),
                        value: Some(vec![b'x'; VALUE_LEN]),
                    }],
                    provenance: Provenance {
                        who: "resp 127.0.0.1:50000".to_owned(),
                        why: "SET".to_owned(),
                        approved_by: None,
                    },
                })
                .collect::<Vec<_>>();
            written += requests.len();
            for result in store.commit_all(requests).expect("writes the group") {
                result.expect("commits");
            }
        }
    }
    println!("{} records in {}", store.log_records(), log.display());
}

/// The resident set of this process in bytes.
fn resident() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("runs ps");
    let kib: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("ps prints a number");
    kib * 1024
}

fn open(holds: &str, keys: usize, log: &Path) {
    let storage = FileStorage::open(log).expect("opens log");
    let started = Instant::now();
    let store: FileStore = match holds {
        "history" => Store::open(storage, SystemClock),
        "current" => Store::open_current_values(storage, None, SystemClock),
        other => panic!("a replica holds history or current, not {other}"),
    }
    .expect("opens store");
    let opened = started.elapsed();
    let held = resident();

    // Read as a server's threads do, from a view.
    let view = store.view();
    let mut rng = Rng::new();
    let mut found = 0;
    let started = Instant::now();
    for _ in 0..READS {
        found += usize::from(
            view.get(MAIN_BRANCH, &key(rng.next() % keys as u64))
                .is_some(),
        );
    }
    let per_read = started.elapsed().as_nanos() / READS as u128;

    println!("holds,records,open_ms,rss_after_open_bytes,ns_per_read,reads_found");
    println!(
        "{holds},{},{},{held},{per_read},{found}",
        store.log_records(),
        opened.as_millis()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |at: usize| -> usize { args[at].parse().expect("a number") };
    match args.first().map(String::as_str) {
        Some("make") => make(number(1), number(2), Path::new(&args[3])),
        Some(holds @ ("history" | "current")) => open(holds, number(1), Path::new(&args[2])),
        _ => panic!("make <keys> <passes> <log>, or history|current <keys> <log>"),
    }
}
