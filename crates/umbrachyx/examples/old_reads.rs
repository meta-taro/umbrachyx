//! What reading the past costs once most commits keep only the cells they
//! wrote: the diff of an old commit against its parent, against the fifth
//! and the thousandth commit before it, one key's value at an old commit,
//! and the whole state an old commit held.
//!
//! `cargo run --release --example old_reads [-- <commits>]`
//!
//! Writes `<commits>` commits of one random key each, 32-byte values over
//! a space of as many keys, in groups of 64 as a pipelining client sends
//! them, to a fresh log under the temp dir. Then times diffs at commits
//! spread over the whole history. Record the machine alongside the output.

use std::time::Instant;

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{CommitId, CommitRequest, MAIN_BRANCH, Provenance, Store, SystemClock};

const DEFAULT_COMMITS: usize = 200_000;
const GROUP: usize = 64;
const PROBES: usize = 300;
/// Reads of a whole past state, which take far longer each.
const WHOLE_PROBES: usize = 20;

type FileStore = Store<FileStorage, SystemClock>;

/// A small deterministic generator, so runs write the same keys.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn write(store: &mut FileStore, commits: usize) -> Vec<CommitId> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut ids = Vec::with_capacity(commits);
    while ids.len() < commits {
        let requests = (0..GROUP.min(commits - ids.len()))
            .map(|_| CommitRequest {
                branch: MAIN_BRANCH.to_owned(),
                edits: vec![Edit {
                    key: format!("key:{:012}", rng.next() % commits as u64).into_bytes(),
                    value: Some(rng.next().to_le_bytes().repeat(4)),
                }],
                provenance: Provenance {
                    who: "old_reads".to_owned(),
                    why: String::new(),
                    approved_by: None,
                },
            })
            .collect();
        for answer in store.commit_all(requests).expect("writes") {
            ids.push(answer.expect("commits"));
        }
    }
    ids
}

/// Mean and slowest time in milliseconds of a diff from `back` commits
/// before each probed commit to it.
fn time_diffs(store: &FileStore, ids: &[CommitId], back: usize) -> (f64, f64) {
    let (mut total, mut slowest) = (0.0_f64, 0.0_f64);
    for probe in 0..PROBES {
        let at = back + (probe * 7919) % (ids.len() - back);
        let started = Instant::now();
        store
            .diff(Some(&ids[at - back]), Some(&ids[at]))
            .expect("diffs");
        let ms = started.elapsed().as_secs_f64() * 1e3;
        total += ms;
        slowest = slowest.max(ms);
    }
    (total / PROBES as f64, slowest)
}

/// Mean and slowest time in milliseconds of `read` at commits spread over
/// the whole history, `probes` of them.
fn time_reads(ids: &[CommitId], probes: usize, read: impl Fn(usize, &CommitId)) -> (f64, f64) {
    let (mut total, mut slowest) = (0.0_f64, 0.0_f64);
    for probe in 0..probes {
        let at = (probe * 7919) % ids.len();
        let started = Instant::now();
        read(at, &ids[at]);
        let ms = started.elapsed().as_secs_f64() * 1e3;
        total += ms;
        slowest = slowest.max(ms);
    }
    (total / probes as f64, slowest)
}

fn main() {
    let commits = std::env::args().nth(1).map_or(DEFAULT_COMMITS, |arg| {
        arg.parse().expect("a number of commits")
    });
    let path = std::env::temp_dir().join(format!("umbrachyx-old-reads-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut store =
        Store::open(FileStorage::open(&path).expect("opens log"), SystemClock).expect("opens");
    let ids = write(&mut store, commits);

    println!("| Diff, {commits} commits | mean (ms) | slowest (ms) |");
    println!("|---|---|---|");
    for (label, back) in [
        ("against its parent", 1),
        ("against the fifth before", 5),
        ("against the thousandth before", 1000),
    ] {
        let (mean, slowest) = time_diffs(&store, &ids, back);
        println!("| an old commit {label} | {mean:.3} | {slowest:.3} |");
    }
    let (mean, slowest) = time_reads(&ids, PROBES, |at, id| {
        let key = format!("key:{:012}", (at * 31) % commits);
        store.get_at(id, key.as_bytes()).expect("reads");
    });
    println!("| one key's value at an old commit | {mean:.3} | {slowest:.3} |");
    let (mean, slowest) = time_reads(&ids, WHOLE_PROBES, |_, id| {
        store.diff(None, Some(id)).expect("diffs");
    });
    println!("| everything an old commit held, against nothing | {mean:.3} | {slowest:.3} |");
    drop(store);
    let _ = std::fs::remove_file(&path);
}
