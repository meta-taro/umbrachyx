//! GETs from many threads at once on one view, in process: keys that keep
//! their history against keys in a table declared `Latest`. It asks whether
//! a lookup in the latest-value map is what makes reads from 50 clients
//! slower over the server (`docs/comparison.md`), with the network, the
//! threads per connection and the client taken out.
//!
//! `cargo run --release --example read_contention [-- <keys> <threads> <reads> [plain]]`
//!
//! With `plain`, values are not encrypted, so a read takes no lock on the
//! store's keys.
//!
//! Writes `<keys>` keys of 32-byte values to a fresh, encrypted store under
//! the temp dir, as the server would (a branch, one subject for all keys),
//! then has `<threads>` threads each read `<reads>` random keys through the
//! same view, timing each read. Prints, per engine, the reads a second over
//! all threads and the 50th, 99th and 99.9th percentile of one read in
//! nanoseconds. Record the machine alongside the output.

use std::sync::Arc;
use std::thread;
use std::time::Instant;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock, View};

const DEFAULT_KEYS: usize = 1_000_000;
const DEFAULT_THREADS: usize = 50;
const DEFAULT_READS: usize = 200_000;
/// Keys per commit while filling; a GET reads only the current state, so
/// how the keys were grouped into commits does not matter here.
const PER_COMMIT: usize = 1_000;
const VALUE_LEN: usize = 32;
const BRANCH: &str = "bench";

/// A small deterministic generator, so both engines read the same keys.
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
        who: "bench".to_owned(),
        why: "read_contention".to_owned(),
        approved_by: None,
    }
}

/// The key of number `n`: in no table, or in `pos` under one subject, as
/// `bench/umbrachyx.sh` writes them.
fn key(engine: Engine, n: usize) -> Vec<u8> {
    match engine {
        Engine::History => format!("key:{n:012}").into_bytes(),
        Engine::Latest => format!("pos/bench/key:{n:012}").into_bytes(),
    }
}

/// A store with `keys` keys on `BRANCH`, and its view.
fn filled(engine: Engine, keys: usize, dir: &std::path::Path, plain: bool) -> View {
    std::fs::create_dir_all(dir).expect("creates dir");
    let storage = FileStorage::open(&dir.join("log")).expect("opens log");
    let mut store = if plain {
        Store::open(storage, SystemClock).expect("opens store")
    } else {
        let key_storage: Box<dyn KeyStorage + Send> =
            Box::new(FileKeyStorage::new(&dir.join("log.keys")));
        let sealed_keys = Keys::open(key_storage).expect("opens keys");
        Store::open_sealed(storage, sealed_keys, SystemClock).expect("opens store")
    };
    if engine == Engine::Latest {
        store
            .declare_table(b"pos", Engine::Latest, provenance())
            .expect("declares");
    }
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    for start in (0..keys).step_by(PER_COMMIT) {
        let edits = (start..keys.min(start + PER_COMMIT))
            .map(|n| Edit {
                key: key(engine, n),
                value: Some(vec![b'x'; VALUE_LEN]),
            })
            .collect();
        store.commit(BRANCH, edits, provenance()).expect("commits");
    }
    store.view()
}

/// Each thread's read times in nanoseconds, all together, and the seconds
/// the slowest thread took.
fn read_all(
    view: &Arc<View>,
    engine: Engine,
    keys: usize,
    threads: usize,
    reads: usize,
) -> (Vec<u64>, f64) {
    let start = Instant::now();
    let handles: Vec<_> = (0..threads)
        .map(|thread| {
            let view = Arc::clone(view);
            thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (thread as u64 + 1));
                let mut times = Vec::with_capacity(reads);
                for _ in 0..reads {
                    let key = key(engine, (rng.next() % keys as u64) as usize);
                    let began = Instant::now();
                    let value = view.get(BRANCH, &key);
                    times.push(began.elapsed().as_nanos() as u64);
                    assert!(value.is_some(), "every key was written");
                }
                times
            })
        })
        .collect();
    let mut times: Vec<u64> = handles
        .into_iter()
        .flat_map(|handle| handle.join().expect("a reader finishes"))
        .collect();
    let seconds = start.elapsed().as_secs_f64();
    times.sort_unstable();
    (times, seconds)
}

fn percentile(sorted: &[u64], per_mille: usize) -> u64 {
    sorted[(sorted.len() - 1) * per_mille / 1000]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |at: usize, default: usize| {
        args.get(at)
            .map_or(default, |arg| arg.parse().expect("a number"))
    };
    let keys = number(0, DEFAULT_KEYS);
    let threads = number(1, DEFAULT_THREADS);
    let reads = number(2, DEFAULT_READS);
    let plain = args.get(3).is_some_and(|arg| arg == "plain");
    let root =
        std::env::temp_dir().join(format!("umbrachyx-read-contention-{}", std::process::id()));

    println!("engine,keys,threads,reads_per_thread,reads_per_second,p50_ns,p99_ns,p999_ns");
    for (engine, name) in [(Engine::History, "history"), (Engine::Latest, "latest")] {
        let view = Arc::new(filled(engine, keys, &root.join(name), plain));
        let (times, seconds) = read_all(&view, engine, keys, threads, reads);
        let rate = times.len() as f64 / seconds;
        println!(
            "{name},{keys},{threads},{reads},{rate:.0},{},{},{}",
            percentile(&times, 500),
            percentile(&times, 990),
            percentile(&times, 999)
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
