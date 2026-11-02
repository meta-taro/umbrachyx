//! Durable writes from many writer threads at once, through the committer
//! (`docs/comparison.md`). Prints a Markdown table.
//!
//! ```text
//! cargo run --release -p umbrachyx --example concurrent_writes
//! ```
//!
//! Each writer commits one key at a time and waits until it is durable
//! before the next, as a client of a server would. Everything runs in this
//! process, so no number includes a network round trip.

use std::sync::Arc;
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock, Writer};

const WRITERS: [usize; 4] = [1, 8, 64, 256];
/// Commits per run, split over the writers: enough syncs for a p99 even
/// when one writer waits for every sync on its own.
const COMMITS: usize = 2_048;

type Error = Box<dyn std::error::Error>;

fn main() -> Result<(), Error> {
    let work = std::env::temp_dir().join(format!("umbrachyx-concurrent-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    println!("| Writers | Commits per second | p50 (ms) | p99 (ms) |");
    println!("|---|---|---|---|");
    let result = WRITERS
        .iter()
        .try_for_each(|&writers| -> Result<(), Error> {
            let log = work.join(format!("log-{writers}"));
            let store = Store::open(FileStorage::open(&log)?, SystemClock)?;
            let (per_second, p50, p99) = run(Committer::start(store), writers);
            println!(
                "| {writers} | {per_second:.0} | {:.2} | {:.2} |",
                millis(p50),
                millis(p99)
            );
            Ok(())
        });
    std::fs::remove_dir_all(&work)?;
    result
}

fn run(
    committer: Committer<FileStorage, SystemClock>,
    writers: usize,
) -> (f64, Duration, Duration) {
    let each = COMMITS / writers;
    let start = Arc::new(Barrier::new(writers + 1));
    let threads: Vec<_> = (0..writers)
        .map(|writer| {
            let handle = committer.writer();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                (0..each)
                    .map(|n| commit(&handle, writer, n))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    start.wait();
    let started = Instant::now();
    let mut times: Vec<Duration> = threads
        .into_iter()
        .flat_map(|thread| thread.join().expect("writer finishes"))
        .collect();
    let elapsed = started.elapsed();
    drop(committer.stop());
    times.sort();
    let at = |fraction: f64| times[((times.len() - 1) as f64 * fraction) as usize];
    let per_second = times.len() as f64 / elapsed.as_secs_f64();
    (per_second, at(0.50), at(0.99))
}

fn commit(handle: &Writer, writer: usize, n: usize) -> Duration {
    let edit = Edit {
        key: format!("player/{writer:08}/slot/{:03}", n % 1_000).into_bytes(),
        value: Some(format!("{n:032}").into_bytes()),
    };
    let provenance = Provenance {
        who: "bench".to_owned(),
        why: "concurrent write".to_owned(),
        approved_by: None,
    };
    let started = Instant::now();
    handle
        .commit(MAIN_BRANCH, vec![edit], provenance)
        .expect("commits");
    started.elapsed()
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}
