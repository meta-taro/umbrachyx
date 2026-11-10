//! Memory per SET in a table that keeps its history and in one declared
//! `Latest`, in the same shape as `bench/memory.sh`: two passes over the
//! same random keys, and the growth of the resident set during the second.
//!
//! `cargo run --release --example engine_memory [-- <history|latest> <sets>]`
//!
//! Writes `<sets>` commits of one key each, 32-byte values over a space of
//! as many keys, in groups of 64 as a pipelining client sends them, to a
//! fresh log under the temp dir; then the same again. The resident set is
//! read with `ps`, so this runs on macOS and Linux. Record the machine
//! alongside the output.

use std::process::Command;

use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{CommitRequest, MAIN_BRANCH, Provenance, Store, SystemClock};

const DEFAULT_SETS: usize = 200_000;
const GROUP: usize = 64;
const VALUE_LEN: usize = 32;

type FileStore = Store<FileStorage, SystemClock>;

/// A small deterministic generator, so both passes write the same keys.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn provenance() -> Provenance {
    Provenance {
        who: "resp 127.0.0.1:50000".to_owned(),
        why: "SET".to_owned(),
        approved_by: None,
    }
}

fn pass(store: &mut FileStore, table: &str, sets: usize) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut written = 0;
    while written < sets {
        let requests = (0..GROUP.min(sets - written))
            .map(|_| CommitRequest {
                branch: MAIN_BRANCH.to_owned(),
                edits: vec![Edit {
                    key: format!("{table}/{}", rng.next() % sets as u64).into_bytes(),
                    value: Some(vec![b'x'; VALUE_LEN]),
                }],
                provenance: provenance(),
            })
            .collect::<Vec<_>>();
        written += requests.len();
        for result in store.commit_all(requests).expect("writes the group") {
            result.expect("commits");
        }
    }
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

fn main() {
    let mut args = std::env::args().skip(1);
    let engine = args.next().unwrap_or_else(|| "latest".to_owned());
    let sets: usize = args
        .next()
        .map_or(DEFAULT_SETS, |n| n.parse().expect("a number of sets"));
    let (table, declared) = match engine.as_str() {
        "history" => ("player", None),
        "latest" => ("pos", Some(Engine::Latest)),
        other => panic!("engine is history or latest, not {other}"),
    };
    let dir = std::env::temp_dir().join(format!("umbrachyx-engine-memory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creates dir");
    let storage = FileStorage::open(&dir.join("log")).expect("opens log");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    if let Some(engine) = declared {
        store
            .declare_table(table.as_bytes(), engine, provenance())
            .expect("declares");
    }
    let start = resident();
    pass(&mut store, table, sets);
    let first = resident();
    pass(&mut store, table, sets);
    let second = resident();
    let _ = std::fs::remove_dir_all(&dir);

    let per_set = second.saturating_sub(first) as f64 / sets as f64;
    println!("engine,sets,rss_start,rss_after_1,rss_after_2,bytes_per_set_pass_2");
    println!("{engine},{sets},{start},{first},{second},{per_set:.0}");
}
