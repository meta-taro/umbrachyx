//! The Umbrachyx side of the comparison with Valkey and Dolt
//! (`docs/comparison.md`). Prints a Markdown table.
//!
//! ```text
//! cargo run --release -p umbrachyx --example compare -- 1000000 [dolt-dir]
//! ```
//!
//! With `dolt-dir`, also writes the same records as `inventory.csv` and the
//! same ten changes as `changes.sql`, for `bench/dolt.sh` to load into Dolt.
//!
//! Everything runs in this process: there is no Umbrachyx server yet, so no
//! number here includes a network round trip.

use std::hint::black_box;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock};

/// Records per commit while loading. A commit is one log frame, which has a
/// size limit; this keeps each one well under it.
const LOAD_BATCH: usize = 50_000;
const LOOKUPS: usize = 100_000;
const SINGLE_COMMITS: usize = 1_000;
const DIFF_CHANGES: usize = 10;
const DIFF_RUNS: usize = 1_001;
/// Diffs run untimed for this long first. The single commits before them
/// each wait about 4 ms on the disk, which leaves the core slow for a while.
const DIFF_WARM_UP: Duration = Duration::from_millis(200);
const SEED: u64 = 2026;
const BRANCH: &str = "event/test";
/// The same ten changes in two commits of five. A commit against its parent
/// is read from the cells it wrote; across two commits the diff walks the
/// trees, which is what Dolt's diff of two branches does.
const TWO_COMMIT_BRANCH: &str = "event/test-in-two";

type FileStore = Store<FileStorage, SystemClock>;
type Error = Box<dyn std::error::Error>;

/// SplitMix64, so the data is the same on every run and in every tool.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// A 32-character hex value: printable, so CSV and SQL carry it as is.
    fn value(&mut self) -> String {
        format!("{:016x}{:016x}", self.next(), self.next())
    }
}

fn key(index: u64) -> String {
    format!("player/{:08}/slot/{:03}", index / 100, 2 * (index % 100))
}

/// A key between existing ones: an odd slot.
fn new_key(index: u64) -> String {
    format!(
        "player/{:08}/slot/{:03}",
        index / 100,
        2 * (index % 100) + 1
    )
}

fn by(why: &str) -> Provenance {
    Provenance {
        who: "bench".to_owned(),
        why: why.to_owned(),
        approved_by: None,
    }
}

fn main() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let records: u64 = args.next().map_or(Ok(1_000_000), |n| n.parse())?;
    let dolt_dir = args.next().map(PathBuf::from);
    let work = std::env::temp_dir().join(format!("umbrachyx-compare-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    let result = run(records, &work, dolt_dir.as_deref());
    std::fs::remove_dir_all(&work)?;
    result
}

fn run(records: u64, work: &Path, dolt_dir: Option<&Path>) -> Result<(), Error> {
    let mut rng = Rng(SEED);
    let mut store = Store::open(FileStorage::open(&work.join("log"))?, SystemClock)?;

    let started = Instant::now();
    load(&mut store, &mut rng, records)?;
    let load = started.elapsed();

    let get = percentiles((0..LOOKUPS).map(|_| {
        let key = key(rng.below(records));
        let started = Instant::now();
        black_box(store.get(MAIN_BRANCH, key.as_bytes()));
        started.elapsed()
    }));

    let mut commit_times = Vec::with_capacity(SINGLE_COMMITS);
    for _ in 0..SINGLE_COMMITS {
        let edit = set(&key(rng.below(records)), &rng.value());
        let started = Instant::now();
        store.commit(MAIN_BRANCH, vec![edit], by("single write"))?;
        commit_times.push(started.elapsed());
    }
    let commit = percentiles(commit_times.into_iter());

    if let Some(dir) = dolt_dir {
        write_dolt_records(dir, &store, records)?;
    }
    let changes = ten_changes(&mut rng, records);
    store.create_branch(BRANCH, MAIN_BRANCH)?;
    store.commit(
        BRANCH,
        changes.iter().map(|(k, v)| set(k, v)).collect(),
        by("ten changes"),
    )?;
    if let Some(dir) = dolt_dir {
        write_dolt_changes(dir, &changes)?;
    }
    let diff = median_diff(&store, BRANCH, changes.len())?;

    store.create_branch(TWO_COMMIT_BRANCH, MAIN_BRANCH)?;
    for half in changes.chunks(DIFF_CHANGES / 2) {
        store.commit(
            TWO_COMMIT_BRANCH,
            half.iter().map(|(k, v)| set(k, v)).collect(),
            by("five changes"),
        )?;
    }
    let tree_diff = median_diff(&store, TWO_COMMIT_BRANCH, changes.len())?;

    println!(
        "| Records | Load (s) | get p50 / p99 (µs) | commit of one key p50 / p99 (ms) \
         | diff of 10 changes in one commit, median (µs) \
         | diff of 10 changes in two commits, median (µs) |"
    );
    println!("|---|---|---|---|---|---|");
    println!(
        "| {records} | {:.1} | {:.2} / {:.2} | {:.2} / {:.2} | {:.2} | {:.2} |",
        load.as_secs_f64(),
        micros(get.0),
        micros(get.1),
        micros(commit.0) / 1_000.0,
        micros(commit.1) / 1_000.0,
        micros(diff),
        micros(tree_diff),
    );
    Ok(())
}

fn load(store: &mut FileStore, rng: &mut Rng, records: u64) -> Result<(), Error> {
    let mut index = 0;
    while index < records {
        let end = (index + LOAD_BATCH as u64).min(records);
        let edits = (index..end).map(|i| set(&key(i), &rng.value())).collect();
        store.commit(MAIN_BRANCH, edits, by("load"))?;
        index = end;
    }
    Ok(())
}

fn median_diff(store: &FileStore, branch: &str, expected: usize) -> Result<Duration, Error> {
    let (main, branch) = (store.head(MAIN_BRANCH), store.head(branch));
    let warm_up = Instant::now();
    while warm_up.elapsed() < DIFF_WARM_UP {
        black_box(store.diff(main.as_ref(), branch.as_ref())?);
    }
    let mut times = Vec::with_capacity(DIFF_RUNS);
    for _ in 0..DIFF_RUNS {
        let started = Instant::now();
        let diff = store.diff(main.as_ref(), branch.as_ref())?;
        times.push(started.elapsed());
        assert_eq!(diff.len(), expected, "the diff missed changes");
    }
    times.sort();
    Ok(times[DIFF_RUNS / 2])
}

/// Five updates of existing records and five new records, all distinct.
fn ten_changes(rng: &mut Rng, records: u64) -> Vec<(String, String)> {
    let mut changes = std::collections::BTreeMap::new();
    while changes.len() < DIFF_CHANGES {
        let index = rng.below(records);
        let key = if changes.len() % 2 == 0 {
            key(index)
        } else {
            new_key(index)
        };
        changes.insert(key, rng.value());
    }
    changes.into_iter().collect()
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

/// The records as `main` holds them before the branch, so Dolt starts from
/// exactly the same state.
fn write_dolt_records(dir: &Path, store: &FileStore, records: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut csv = std::io::BufWriter::new(std::fs::File::create(dir.join("inventory.csv"))?);
    writeln!(csv, "k,v")?;
    for index in 0..records {
        let key = key(index);
        let value = store
            .get(MAIN_BRANCH, key.as_bytes())
            .expect("every loaded record is there");
        writeln!(csv, "{key},{}", String::from_utf8_lossy(&value))?;
    }
    csv.flush()
}

/// The same ten changes, each an upsert.
fn write_dolt_changes(dir: &Path, changes: &[(String, String)]) -> std::io::Result<()> {
    let mut sql = std::fs::File::create(dir.join("changes.sql"))?;
    for (key, value) in changes {
        writeln!(
            sql,
            "REPLACE INTO inventory (k, v) VALUES ('{key}', '{value}');"
        )?;
    }
    Ok(())
}

fn percentiles(samples: impl Iterator<Item = Duration>) -> (Duration, Duration) {
    let mut samples: Vec<Duration> = samples.collect();
    samples.sort();
    let at = |fraction: f64| samples[((samples.len() - 1) as f64 * fraction) as usize];
    (at(0.50), at(0.99))
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}
