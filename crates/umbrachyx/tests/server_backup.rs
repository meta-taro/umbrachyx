//! A server that backs its store up as it serves: the backup holds what
//! clients wrote, clients are answered while it is taken, and a backup
//! that cannot be kept does not stop the server.
//!
//! The data is synthetic: players and their gems.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::server::Server;
use umbrachyx::store::backup::{self, DirSource, DirTarget, restore};
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};

const BRANCH: &str = "bench";
const EVERY: Duration = Duration::from_millis(20);
/// How long a backup gets to hold what was written. It takes a few
/// milliseconds; this only has to be longer than a busy machine makes it.
const LIMIT: Duration = Duration::from_secs(30);

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-backup-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn backup(&self) -> PathBuf {
        self.0.join("backup")
    }

    /// The store the backup holds now, restored to a place of its own,
    /// or `None` if no backup has finished.
    fn restored(&self, name: &str) -> Option<FileStore> {
        let path = self.0.join(name);
        let _ = std::fs::remove_file(&path);
        let mut log = FileStorage::open(&path).expect("opens file");
        restore(&mut DirSource::new(&self.backup()), &mut log, None).ok()?;
        Some(Store::open(log, SystemClock).expect("opens the restored store"))
    }

    /// Waits for a backup that holds `key` as `value` on the branch
    /// served.
    fn wait_for_a_backup_with(&self, key: &str, value: &str) {
        let start = Instant::now();
        loop {
            let held = self
                .restored("restored")
                .and_then(|store| store.get(BRANCH, key.as_bytes()).map(|held| held.to_vec()));
            if held.as_deref() == Some(value.as_bytes()) {
                return;
            }
            assert!(
                start.elapsed() < LIMIT,
                "no backup held {key} = {value} within {LIMIT:?}; it held {held:?}"
            );
            thread::sleep(EVERY);
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn store(path: &Path) -> FileStore {
    let storage = FileStorage::open(path).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    store
}

/// Serves `BRANCH` of `store` on a free port of this machine, backing it
/// up to `backup`.
fn serve(store: FileStore, backup: &Path) -> TcpStream {
    let committer = Committer::start(store);
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .backup_to(backup.to_owned(), EVERY);
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    thread::spawn(move || {
        // Kept for as long as the server runs: stopping it would end
        // the store under the server.
        let _committer = committer;
        server.serve(listener)
    });
    TcpStream::connect(address).expect("connects")
}

fn set(client: &mut TcpStream, key: &str, value: &str) {
    let command = format!(
        "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{value}\r\n",
        key.len(),
        value.len()
    );
    client.write_all(command.as_bytes()).expect("sends");
    let mut reply = [0; 5];
    client.read_exact(&mut reply).expect("reads");
    assert_eq!(&reply, b"+OK\r\n");
}

#[test]
fn a_server_backs_its_store_up_as_it_serves() {
    let dir = TempDir::new("serves");
    let mut client = serve(store(&dir.0.join("log")), &dir.backup());

    set(&mut client, "player/1/gems", "100");
    dir.wait_for_a_backup_with("player/1/gems", "100");
    set(&mut client, "player/1/gems", "150");
    set(&mut client, "player/2/gems", "7");

    dir.wait_for_a_backup_with("player/2/gems", "7");
    let restored = dir.restored("restored").expect("there is a backup");
    assert_eq!(
        restored.get(BRANCH, b"player/1/gems").as_deref(),
        Some(&b"150"[..])
    );
    assert_eq!(restored.log(BRANCH).len(), 3);
}

#[test]
fn a_backup_that_cannot_be_kept_does_not_stop_the_server() {
    let dir = TempDir::new("another");
    // The place holds the backup of another store.
    let mut other = store(&dir.0.join("other"));
    let mut target = DirTarget::open(&dir.backup()).expect("opens the backup");
    other.backup(&mut target).expect("backs up");
    drop(target);
    let before = std::fs::read(dir.backup().join("manifest")).expect("reads");
    let mut client = serve(store(&dir.0.join("log")), &dir.backup());

    for round in 0..50 {
        set(&mut client, "player/1/gems", &round.to_string());
        thread::sleep(Duration::from_millis(2));
    }

    // The other store's backup is as it was.
    let after = std::fs::read(dir.backup().join("manifest")).expect("reads");
    assert_eq!(after, before);
    let restored = dir.restored("restored").expect("there is a backup");
    assert_eq!(restored.get(BRANCH, b"player/1/gems"), None);
}

#[test]
fn a_place_that_has_gone_is_not_made_again() {
    let dir = TempDir::new("gone");
    let mut client = serve(store(&dir.0.join("log")), &dir.backup());
    set(&mut client, "player/1/gems", "100");
    dir.wait_for_a_backup_with("player/1/gems", "100");

    std::fs::remove_dir_all(dir.backup()).expect("removes the backup");
    for round in 0..50 {
        set(&mut client, "player/1/gems", &round.to_string());
        thread::sleep(Duration::from_millis(2));
    }

    assert!(!dir.backup().exists());
}

/// How long each of the SETs sent one after another took, until told
/// to stop.
fn set_until(mut client: TcpStream, is_done: Arc<AtomicBool>) -> Vec<Duration> {
    let mut took = Vec::new();
    while !is_done.load(Ordering::Acquire) {
        let sent = Instant::now();
        set(&mut client, "player/1/gems", &took.len().to_string());
        took.push(sent.elapsed());
    }
    took
}

fn shown(mut took: Vec<Duration>) -> String {
    took.sort();
    format!(
        "{} SETs, half within {:?}, the slowest {:?}",
        took.len(),
        took[took.len() / 2],
        took[took.len() - 1]
    )
}

/// A store with a log of some size: `FILLED` commits of `VALUE_LEN` bytes.
fn filled(path: &Path) -> FileStore {
    const VALUE_LEN: usize = 8 * 1024;
    const FILLED: usize = 3_000;
    let mut filled = store(path);
    let value = vec![b'x'; VALUE_LEN];
    for round in 0..FILLED {
        let edit = Edit {
            key: format!("player/{round}/notes").into_bytes(),
            value: Some(value.clone()),
        };
        let provenance = Provenance {
            who: "seed".to_owned(),
            why: "test data".to_owned(),
            approved_by: None,
        };
        filled
            .commit(BRANCH, vec![edit], provenance)
            .expect("commits");
    }
    filled
}

/// A first backup of a log of some size is copied a part at a time, and
/// writers wait for one part to be read at most, never for the copy.
/// Prints how long SETs took meanwhile, beside a server that keeps no
/// backup; run with `--release -- --nocapture` to compare them.
#[test]
fn sets_are_answered_while_a_large_first_backup_is_taken() {
    let dir = TempDir::new("large");
    let alone = {
        let nowhere = TempDir::new("large-alone");
        let committer = Committer::start(filled(&nowhere.0.join("log")));
        let server =
            Server::new(BRANCH, committer.writer(), committer.reader()).expect("serves a branch");
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("has an address");
        thread::spawn(move || {
            let _committer = committer;
            server.serve(listener)
        });
        let is_done = Arc::new(AtomicBool::new(false));
        let client = TcpStream::connect(address).expect("connects");
        let sets = thread::spawn({
            let is_done = Arc::clone(&is_done);
            move || set_until(client, is_done)
        });
        thread::sleep(Duration::from_millis(500));
        is_done.store(true, Ordering::Release);
        sets.join().expect("the client ends")
    };
    let mut store = filled(&dir.0.join("log"));
    // As the `serve` command does before it listens: the place is
    // checked, which also works out the chain over the log as it is.
    backup::check(
        &mut store,
        &mut DirTarget::open(&dir.backup()).expect("opens the backup"),
    )
    .expect("the place can keep the backup");
    let log_len = std::fs::metadata(dir.0.join("log")).expect("stats").len();
    let is_done = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let client = serve(store, &dir.backup());
    let sets = thread::spawn({
        let is_done = Arc::clone(&is_done);
        move || set_until(client, is_done)
    });

    while dir.restored("probe").is_none() {
        assert!(
            start.elapsed() < LIMIT,
            "no backup finished within {LIMIT:?}"
        );
        thread::sleep(EVERY);
    }
    let first_backup = start.elapsed();
    is_done.store(true, Ordering::Release);
    let meanwhile = sets.join().expect("the client ends");

    println!(
        "first backup of {} MiB within {first_backup:?}\n  meanwhile: {}\n  no backup: {}",
        log_len / (1024 * 1024),
        shown(meanwhile.clone()),
        shown(alone)
    );
    assert!(!meanwhile.is_empty());
    dir.wait_for_a_backup_with("player/1/gems", &(meanwhile.len() - 1).to_string());
}
