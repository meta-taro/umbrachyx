//! Measures the tree candidates on the same synthetic data and prints
//! Markdown tables.
//!
//! ```text
//! cargo run --release -p umbrachyx-tree-candidates --example tree_bench
//! TREE_BENCH_SIZES=100000,1000000 cargo run --release ...   # fewer sizes
//! ```
//!
//! The question it answers: does the cost of a diff follow the number of
//! changes, or the number of records? A diff is timed twice. "Derived" is
//! against a version made from the base by inserts, so the two share nodes.
//! "Rebuilt" is against a tree built separately from the same final data, as
//! a replica or a branch built elsewhere would be.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

use umbrachyx_tree_candidates::btree::BPlusTree;
use umbrachyx_tree_candidates::data::{self, Rng};
use umbrachyx_tree_candidates::hamt::HamtTree;
use umbrachyx_tree_candidates::prolly::ProllyTree;
use umbrachyx_tree_candidates::{Bytes, Tree};

const DEFAULT_SIZES: &[u64] = &[100_000, 1_000_000, 10_000_000];
const LOOKUPS: usize = 100_000;
const INSERTS: usize = 10_000;
const CLONES: u32 = 10_000;
const DIFF_CHANGES: usize = 10;
const DIFF_RUNS: usize = 5;
/// Change counts for the second table, at the second size.
const CHANGE_COUNTS: &[usize] = &[10, 100, 1_000, 10_000];
const SEED: u64 = 2026;

type Entries = Vec<(Bytes, Bytes)>;

struct Row {
    name: &'static str,
    records: u64,
    build: Duration,
    heap_bytes: usize,
    get: (Duration, Duration),
    insert: (Duration, Duration),
    clone: Duration,
    derived: DiffRun,
    rebuilt: DiffRun,
}

struct DiffRun {
    time: Duration,
    nodes: usize,
}

fn main() {
    let sizes = sizes();
    let scaling_size = sizes[1.min(sizes.len() - 1)];
    let mut rows = Vec::new();
    let mut scaling = Vec::new();
    for &records in &sizes {
        let entries = data::dataset(records, &mut Rng::new(SEED));
        rows.push(measure::<ProllyTree>(&entries));
        rows.push(measure::<BPlusTree>(&entries));
        rows.push(measure::<HamtTree>(&entries));
        if records == scaling_size {
            scaling.extend(by_change_count::<ProllyTree>(&entries));
            scaling.extend(by_change_count::<BPlusTree>(&entries));
            scaling.extend(by_change_count::<HamtTree>(&entries));
        }
    }
    print_rows(&rows);
    print_scaling(scaling_size, &scaling);
}

fn sizes() -> Vec<u64> {
    match std::env::var("TREE_BENCH_SIZES") {
        Ok(list) => list
            .split(',')
            .map(|size| {
                size.trim()
                    .parse()
                    .expect("TREE_BENCH_SIZES: comma-separated counts")
            })
            .collect(),
        Err(_) => DEFAULT_SIZES.to_vec(),
    }
}

fn measure<T: Tree>(entries: &Entries) -> Row {
    let records = entries.len() as u64;
    eprintln!("{} with {records} records", T::NAME);
    let mut rng = Rng::new(SEED + records);

    let input = entries.clone();
    let started = Instant::now();
    let tree = T::from_sorted(input);
    let build = started.elapsed();

    let get = percentiles((0..LOOKUPS).map(|_| {
        let key = data::key(rng.below(records));
        let started = Instant::now();
        black_box(tree.get(&key));
        started.elapsed()
    }));

    let mut version = tree.clone();
    let insert = percentiles((0..INSERTS).map(|i| {
        let (key, value) = change(&mut rng, records, i);
        let started = Instant::now();
        version = version.insert(key, value);
        started.elapsed()
    }));
    drop(version);

    let started = Instant::now();
    for _ in 0..CLONES {
        black_box(tree.clone());
    }
    let clone = started.elapsed() / CLONES;

    let (derived, rebuilt) = diff_runs(&tree, entries, &mut rng, DIFF_CHANGES);
    Row {
        name: T::NAME,
        records,
        build,
        heap_bytes: tree.heap_bytes(),
        get,
        insert,
        clone,
        derived,
        rebuilt,
    }
}

/// Times a diff against a derived version and against a rebuilt tree, both
/// holding the base plus `count` changes to distinct keys.
fn diff_runs<T: Tree>(
    tree: &T,
    entries: &Entries,
    rng: &mut Rng,
    count: usize,
) -> (DiffRun, DiffRun) {
    let records = entries.len() as u64;
    let mut changes = BTreeMap::new();
    while changes.len() < count {
        let (key, value) = change(rng, records, changes.len());
        changes.insert(key, value);
    }
    let derived = changes.iter().fold(tree.clone(), |version, (key, value)| {
        version.insert(key.clone(), value.clone())
    });
    let rebuilt = T::from_sorted(with_changes(entries, &changes));
    (
        time_diff(tree, &derived, count),
        time_diff(tree, &rebuilt, count),
    )
}

fn time_diff<T: Tree>(old: &T, new: &T, expected: usize) -> DiffRun {
    let mut times = Vec::with_capacity(DIFF_RUNS);
    let mut nodes = 0;
    for _ in 0..DIFF_RUNS {
        let started = Instant::now();
        let diff = old.diff(new);
        times.push(started.elapsed());
        assert_eq!(
            diff.changes.len(),
            expected,
            "{}: the diff missed changes",
            T::NAME
        );
        nodes = diff.nodes_visited;
    }
    times.sort();
    DiffRun {
        time: times[times.len() / 2],
        nodes,
    }
}

fn by_change_count<T: Tree>(entries: &Entries) -> Vec<(&'static str, usize, DiffRun, DiffRun)> {
    let tree = T::from_sorted(entries.clone());
    let mut rng = Rng::new(SEED);
    CHANGE_COUNTS
        .iter()
        .map(|&count| {
            eprintln!("{} with {count} changes", T::NAME);
            let (derived, rebuilt) = diff_runs(&tree, entries, &mut rng, count);
            (T::NAME, count, derived, rebuilt)
        })
        .collect()
}

/// Even-numbered changes update an existing record, odd ones add a record
/// between existing ones.
fn change(rng: &mut Rng, records: u64, i: usize) -> (Bytes, Bytes) {
    let key = if i.is_multiple_of(2) {
        data::key(rng.below(records))
    } else {
        data::new_key(rng, records)
    };
    (key, data::value(rng))
}

/// The entries the base would hold after `changes`, sorted, for building a
/// tree separately.
fn with_changes(entries: &Entries, changes: &BTreeMap<Bytes, Bytes>) -> Entries {
    let mut merged = Vec::with_capacity(entries.len() + changes.len());
    let mut changes = changes.iter().peekable();
    for (key, value) in entries {
        while let Some((new_key, new_value)) = changes.next_if(|(new_key, _)| *new_key < key) {
            merged.push((new_key.clone(), new_value.clone()));
        }
        match changes.next_if(|(new_key, _)| *new_key == key) {
            Some((_, new_value)) => merged.push((key.clone(), new_value.clone())),
            None => merged.push((key.clone(), value.clone())),
        }
    }
    merged.extend(changes.map(|(key, value)| (key.clone(), value.clone())));
    merged
}

fn percentiles(samples: impl Iterator<Item = Duration>) -> (Duration, Duration) {
    let mut samples: Vec<Duration> = samples.collect();
    samples.sort();
    let at = |fraction: f64| samples[((samples.len() - 1) as f64 * fraction) as usize];
    (at(0.50), at(0.99))
}

fn print_rows(rows: &[Row]) {
    println!(
        "| Structure | Records | Build (s) | Heap (MB) | Bytes/record | get p50 / p99 (µs) \
         | insert p50 / p99 (µs) | Branch (ns) | Diff, 10 changes, derived: µs (nodes) \
         | Diff, 10 changes, rebuilt: µs (nodes) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    for row in rows {
        println!(
            "| {} | {} | {:.2} | {} | {} | {:.2} / {:.2} | {:.1} / {:.1} | {} | {:.0} ({}) | {:.0} ({}) |",
            row.name,
            row.records,
            row.build.as_secs_f64(),
            row.heap_bytes / 1_000_000,
            row.heap_bytes as u64 / row.records,
            micros(row.get.0),
            micros(row.get.1),
            micros(row.insert.0),
            micros(row.insert.1),
            row.clone.as_nanos(),
            micros(row.derived.time),
            row.derived.nodes,
            micros(row.rebuilt.time),
            row.rebuilt.nodes,
        );
    }
}

fn print_scaling(records: u64, rows: &[(&'static str, usize, DiffRun, DiffRun)]) {
    println!();
    println!("Diff by number of changes, {records} records:");
    println!();
    println!("| Structure | Changes | Diff, derived: µs (nodes) | Diff, rebuilt: µs (nodes) |");
    println!("|---|---|---|---|");
    for (name, count, derived, rebuilt) in rows {
        println!(
            "| {name} | {count} | {:.0} ({}) | {:.0} ({}) |",
            micros(derived.time),
            derived.nodes,
            micros(rebuilt.time),
            rebuilt.nodes,
        );
    }
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}
