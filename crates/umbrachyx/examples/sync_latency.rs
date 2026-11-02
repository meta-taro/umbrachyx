//! Time from write to "durable" for each way of syncing a file.
//!
//! `cargo run --release --example sync_latency [-- <writes per method>]`
//!
//! Each method appends a record-sized buffer and syncs it, `WRITES` times,
//! in a fresh file under the temp dir. The last row goes through the log
//! itself, so it includes framing and the checksum. Output is a Markdown
//! table; record the machine and disk alongside it, since these numbers mean
//! nothing without them.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use umbrachyx::log::{Edit, FileStorage, Log, Record};

const DEFAULT_WRITES: usize = 2_000;
/// About the size of a small game-state record once framed.
const BUFFER_LEN: usize = 128;

struct Method {
    name: &'static str,
    sync: fn(&File) -> io::Result<()>,
}

fn methods() -> Vec<Method> {
    let std_methods = [
        Method {
            name: SYNC_DATA_NAME,
            sync: File::sync_data,
        },
        Method {
            name: SYNC_ALL_NAME,
            sync: File::sync_all,
        },
    ];
    std_methods.into_iter().chain(plain_fsync()).collect()
}

#[cfg(unix)]
fn plain_fsync() -> Option<Method> {
    Some(Method {
        name: "plain fsync(2)",
        sync: |file| rustix::fs::fsync(file).map_err(io::Error::from),
    })
}

/// Windows has no separate plain fsync: `FlushFileBuffers` is the only sync.
#[cfg(not(unix))]
fn plain_fsync() -> Option<Method> {
    None
}

#[cfg(target_vendor = "apple")]
const SYNC_DATA_NAME: &str = "File::sync_data (F_FULLFSYNC)";
#[cfg(target_vendor = "apple")]
const SYNC_ALL_NAME: &str = "File::sync_all (F_FULLFSYNC)";
#[cfg(target_os = "linux")]
const SYNC_DATA_NAME: &str = "File::sync_data (fdatasync)";
#[cfg(target_os = "linux")]
const SYNC_ALL_NAME: &str = "File::sync_all (fsync)";
#[cfg(windows)]
const SYNC_DATA_NAME: &str = "File::sync_data (FlushFileBuffers)";
#[cfg(windows)]
const SYNC_ALL_NAME: &str = "File::sync_all (FlushFileBuffers)";
#[cfg(not(any(target_vendor = "apple", target_os = "linux", windows)))]
const SYNC_DATA_NAME: &str = "File::sync_data";
#[cfg(not(any(target_vendor = "apple", target_os = "linux", windows)))]
const SYNC_ALL_NAME: &str = "File::sync_all";

struct TempFile(PathBuf);

impl TempFile {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("umbrachyx-sync-{tag}-{}", std::process::id()));
        // A leftover from an earlier run would only make the file longer.
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Best effort: a leftover file in the temp dir harms nothing.
        let _ = std::fs::remove_file(&self.0);
    }
}

fn time_method(method: &Method, writes: usize, path: &Path) -> io::Result<Vec<Duration>> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let buffer = [0x5A; BUFFER_LEN];
    let mut samples = Vec::with_capacity(writes);
    for _ in 0..writes {
        let start = Instant::now();
        file.write_all(&buffer)?;
        (method.sync)(&file)?;
        samples.push(start.elapsed());
    }
    Ok(samples)
}

fn time_log(writes: usize, path: &Path) -> Result<Vec<Duration>, Box<dyn std::error::Error>> {
    let (mut log, _) = Log::open(FileStorage::open(path)?)?;
    let mut samples = Vec::with_capacity(writes);
    for n in 0..writes {
        let record = Record {
            branch: "main".to_owned(),
            changes: vec![Edit {
                key: format!("player:{}:coins", n % 97).into_bytes(),
                value: Some((n as u64).to_le_bytes().to_vec()),
            }],
            who: "agent:bench".to_owned(),
            why: "sync latency".to_owned(),
            approved_by: None,
            time: n as u64,
        };
        let start = Instant::now();
        log.append(&record)?;
        samples.push(start.elapsed());
    }
    Ok(samples)
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let index = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[index]
}

fn row(name: &str, mut samples: Vec<Duration>) -> String {
    samples.sort_unstable();
    let micros = |d: Duration| d.as_secs_f64() * 1e6;
    format!(
        "| {name} | {} | {:.0} | {:.0} | {:.0} |",
        samples.len(),
        micros(percentile(&samples, 50)),
        micros(percentile(&samples, 99)),
        micros(samples[samples.len() - 1]),
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let writes = match std::env::args().nth(1) {
        Some(arg) => arg.parse().map_err(|_| format!("not a count: {arg}"))?,
        None => DEFAULT_WRITES,
    };
    if writes == 0 {
        return Err("writes per method must be at least 1".into());
    }

    println!(
        "{} {} · {} B per write · temp dir {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        BUFFER_LEN,
        std::env::temp_dir().display()
    );
    println!();
    println!("| Method | Writes | p50 (µs) | p99 (µs) | max (µs) |");
    println!("|---|---|---|---|---|");
    for (n, method) in methods().iter().enumerate() {
        let file = TempFile::new(&format!("m{n}"));
        println!(
            "{}",
            row(method.name, time_method(method, writes, &file.0)?)
        );
    }
    let file = TempFile::new("log");
    println!("{}", row("Log::append", time_log(writes, &file.0)?));
    Ok(())
}
