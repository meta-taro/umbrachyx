//! The server writing tables that keep no history: a `SET` to such a
//! table is a put, answered once durable like any other, read back by
//! `GET`, and it makes no commit on the branch the server serves.
//!
//! The data is synthetic: players and their sessions.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use umbrachyx::log::{Engine, FileStorage, Storage};
use umbrachyx::server::Server;
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};

const BRANCH: &str = "bench";

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-plain-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(dir: &TempDir) -> FileStore {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    Store::open(storage, SystemClock).expect("opens store")
}

/// A store with `session` declared plain, and `BRANCH` made from main.
fn store(dir: &TempDir) -> FileStore {
    let mut store = open(dir);
    let provenance = Provenance {
        who: "test".to_owned(),
        why: "sessions need no past".to_owned(),
        approved_by: None,
    };
    store
        .declare_table(b"session", Engine::Plain, provenance)
        .expect("declares");
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    store
}

/// Serves `BRANCH` of `store` on a free port of this machine.
fn serve(store: FileStore) -> (Committer<FileStorage, SystemClock>, TcpStream) {
    serve_syncing_every(store, Duration::from_secs(3600))
}

/// As `serve`, writing puts answered before the disk within `sync_every`.
fn serve_syncing_every<S: Storage + Send + 'static>(
    store: Store<S, SystemClock>,
    sync_every: Duration,
) -> (Committer<S, SystemClock>, TcpStream) {
    let committer = Committer::start(store);
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .sync_every(sync_every);
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    thread::spawn(move || server.serve(listener));
    let client = TcpStream::connect(address).expect("connects");
    (committer, client)
}

fn command(args: &[&str]) -> Vec<u8> {
    let mut bytes = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        bytes.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    bytes
}

/// One line of a reply, without its `\r\n`.
fn read_line(client: &mut TcpStream) -> String {
    let mut line = Vec::new();
    let mut byte = [0];
    while !line.ends_with(b"\r\n") {
        client.read_exact(&mut byte).expect("reads a line");
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).expect("a line is text")
}

fn ask_line(client: &mut TcpStream, args: &[&str]) -> String {
    client.write_all(&command(args)).expect("sends");
    read_line(client)
}

/// Sends one command and reads its reply as JSON in a bulk string.
fn ask_json(client: &mut TcpStream, args: &[&str]) -> Value {
    let line = ask_line(client, args);
    let len: usize = line
        .strip_prefix('$')
        .and_then(|len| len.parse().ok())
        .unwrap_or_else(|| panic!("{args:?} answers a bulk string, not {line:?}"));
    let mut body = vec![0; len + 2];
    client.read_exact(&mut body).expect("reads the body");
    serde_json::from_slice(&body[..len]).expect("the body is JSON")
}

/// What `GET key` answers: the value, or `None` for a key that holds
/// nothing.
fn get(client: &mut TcpStream, key: &str) -> Option<String> {
    let line = ask_line(client, &["GET", key]);
    if line == "$-1" {
        return None;
    }
    let len: usize = line
        .strip_prefix('$')
        .and_then(|len| len.parse().ok())
        .unwrap_or_else(|| panic!("GET answers a bulk string, not {line:?}"));
    let mut body = vec![0; len + 2];
    client.read_exact(&mut body).expect("reads the body");
    Some(String::from_utf8(body[..len].to_vec()).expect("the value is text"))
}

fn text(value: Option<umbrachyx::tree::Bytes>) -> Option<String> {
    value.map(|bytes| String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

#[test]
fn a_set_to_a_plain_table_is_read_back_and_makes_no_commit() {
    let dir = TempDir::new("set");
    let store = store(&dir);
    let head = store.head(BRANCH);
    let (committer, mut client) = serve(store);

    assert_eq!(
        ask_line(&mut client, &["SET", "session/1/token", "a"]),
        "+OK"
    );
    assert_eq!(
        ask_line(&mut client, &["SET", "session/1/token", "b"]),
        "+OK"
    );
    assert_eq!(get(&mut client, "session/1/token"), Some("b".to_owned()));
    assert_eq!(get(&mut client, "session/2/token"), None);

    let store = committer.stop();
    assert_eq!(store.head(BRANCH), head, "a put is not a commit");
    assert_eq!(
        text(store.get(MAIN_BRANCH, b"session/1/token")),
        Some("b".to_owned())
    );
}

/// Sent without waiting, `SET`s to both kinds of table are written
/// together, answered in the order sent, and all there after a restart.
#[test]
fn pipelined_sets_to_both_kinds_of_table_all_land_and_survive_a_restart() {
    const PAIRS: usize = 150;
    let dir = TempDir::new("pipelined");
    let (committer, mut client) = serve(store(&dir));
    let mut request = Vec::new();
    for n in 0..PAIRS {
        request.extend(command(&[
            "SET",
            &format!("player/{n}/gold"),
            &n.to_string(),
        ]));
        request.extend(command(&[
            "SET",
            &format!("session/{n}/token"),
            &format!("t{n}"),
        ]));
    }

    client.write_all(&request).expect("sends");
    for _ in 0..2 * PAIRS {
        assert_eq!(read_line(&mut client), "+OK");
    }
    assert_eq!(
        get(&mut client, "session/149/token"),
        Some("t149".to_owned())
    );
    assert_eq!(get(&mut client, "player/149/gold"), Some("149".to_owned()));
    drop(committer.stop());

    let store = open(&dir);
    for n in 0..PAIRS {
        let gold = text(store.get(BRANCH, format!("player/{n}/gold").as_bytes()));
        let token = text(store.get(BRANCH, format!("session/{n}/token").as_bytes()));
        assert_eq!(gold, Some(n.to_string()));
        assert_eq!(token, Some(format!("t{n}")));
    }
}

/// A viewer lists and opens a plain table's keys like any other: `TYPE`
/// and `TTL` answer as for a string that never expires, and `SCAN`
/// returns them.
#[test]
fn a_viewer_sees_the_keys_of_a_plain_table() {
    let dir = TempDir::new("viewer");
    let (_committer, mut client) = serve(store(&dir));
    assert_eq!(ask_line(&mut client, &["SET", "player/1/gold", "5"]), "+OK");
    assert_eq!(
        ask_line(&mut client, &["SET", "session/1/token", "a"]),
        "+OK"
    );

    assert_eq!(
        ask_line(&mut client, &["TYPE", "session/1/token"]),
        "+string"
    );
    assert_eq!(ask_line(&mut client, &["TTL", "session/1/token"]), ":-1");
    assert_eq!(ask_line(&mut client, &["TYPE", "session/2/token"]), "+none");
    assert_eq!(ask_line(&mut client, &["TTL", "session/2/token"]), ":-2");

    client
        .write_all(&command(&["SCAN", "0", "MATCH", "session/*"]))
        .expect("sends");
    let lines: Vec<String> = (0..6).map(|_| read_line(&mut client)).collect();
    assert_eq!(lines, ["*2", "$1", "0", "*1", "$15", "session/1/token"]);
}

/// Asked for the past of a plain table, the server says there is none
/// rather than answering as if nothing had happened: a key's history is
/// refused, and listings of cells and diffs name the table they left out,
/// between branch heads too, since no commit holds its writes.
#[test]
fn history_cells_and_diffs_say_a_plain_table_has_no_past() {
    let dir = TempDir::new("no-past");
    let (_committer, mut client) = serve(store(&dir));
    assert_eq!(ask_line(&mut client, &["SET", "player/1/gold", "5"]), "+OK");
    assert_eq!(
        ask_line(&mut client, &["SET", "session/1/token", "a"]),
        "+OK"
    );

    assert_eq!(
        ask_line(&mut client, &["UMBRA.HISTORY", BRANCH, "session/1/token"]),
        "-NOHISTORY table 'session' keeps only the latest value"
    );
    let tables = ask_json(&mut client, &["UMBRA.TABLES"]);
    assert_eq!(
        tables["tables"],
        json!([{"name": "session", "engine": "plain"}])
    );
    let cells = ask_json(&mut client, &["UMBRA.CELLS", BRANCH]);
    let keys: Vec<&Value> = cells["cells"]
        .as_array()
        .expect("a list of cells")
        .iter()
        .map(|cell| &cell["key"])
        .collect();
    assert_eq!(keys, ["player/1/gold"]);
    assert_eq!(cells["omitted_tables"], json!(["session"]));
    let diff = ask_json(&mut client, &["UMBRA.DIFF", MAIN_BRANCH, BRANCH]);
    assert_eq!(diff["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(diff["omitted_tables"], json!(["session"]));
}

/// What a power cut would leave of a log: the bytes that were appended
/// and then synced. Kept beside the file because a log held open cannot
/// be read by another handle on every system.
#[derive(Clone, Default)]
struct Disk(Arc<Mutex<Vec<u8>>>);

impl Disk {
    /// The store those bytes open as, in a file of their own.
    fn after_a_power_cut(&self, dir: &TempDir) -> FileStore {
        let copy = dir.0.join("copy");
        let bytes = self.0.lock().expect("no test panics holding it").clone();
        std::fs::write(&copy, bytes).expect("writes the copy");
        let storage = FileStorage::open(&copy).expect("opens file");
        Store::open(storage, SystemClock).expect("opens store")
    }
}

/// A file that tells a [`Disk`] what it has synced.
struct Mirrored {
    file: FileStorage,
    written: Vec<u8>,
    disk: Disk,
}

impl Mirrored {
    fn open(path: &Path) -> (Self, Disk) {
        let mut file = FileStorage::open(path).expect("opens file");
        let written = file.read_all().expect("reads the log");
        let disk = Disk(Arc::new(Mutex::new(written.clone())));
        let mirrored = Self {
            file,
            written,
            disk: disk.clone(),
        };
        (mirrored, disk)
    }
}

impl Storage for Mirrored {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.append(bytes)?;
        self.written.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync()?;
        self.disk
            .0
            .lock()
            .expect("no test panics holding it")
            .clone_from(&self.written);
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.truncate(len)?;
        self.written.truncate(len as usize);
        Ok(())
    }
}

/// A `SET` to a plain-async table is answered and read back before it is
/// on disk, and is on disk within the time the server was given, with no
/// other write to carry it.
#[test]
fn a_set_to_a_plain_async_table_reaches_the_disk_within_the_time_given() {
    let dir = TempDir::new("async");
    let mut store = store(&dir);
    let provenance = Provenance {
        who: "test".to_owned(),
        why: "a cache may lose its last second".to_owned(),
        approved_by: None,
    };
    store
        .declare_table(b"cache", Engine::PlainAsync, provenance)
        .expect("declares");
    drop(store);
    let (storage, disk) = Mirrored::open(&dir.0.join("log"));
    let store = Store::open(storage, SystemClock).expect("opens store");
    let on_disk = |key: &str| {
        text(
            disk.after_a_power_cut(&dir)
                .get(MAIN_BRANCH, key.as_bytes()),
        )
    };
    let (_committer, mut client) = serve_syncing_every(store, Duration::from_millis(300));

    assert_eq!(ask_line(&mut client, &["SET", "cache/top", "1"]), "+OK");
    let answered = Instant::now();
    assert_eq!(get(&mut client, "cache/top"), Some("1".to_owned()));
    let early = on_disk("cache/top");
    if answered.elapsed() < Duration::from_millis(200) {
        assert_eq!(early, None, "answered before it was on disk");
    }

    while on_disk("cache/top").is_none() {
        assert!(
            answered.elapsed() < Duration::from_secs(10),
            "never reached the disk"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// The store `a_set_to_a_plain_async_table…` uses: a `cache` table that
/// is plain-async beside the plain `session`, opened so that what is on
/// disk can be asked.
fn store_with_a_cache(dir: &TempDir) -> (Store<Mirrored, SystemClock>, Disk) {
    let mut store = store(dir);
    let provenance = Provenance {
        who: "test".to_owned(),
        why: "a cache may lose its last second".to_owned(),
        approved_by: None,
    };
    store
        .declare_table(b"cache", Engine::PlainAsync, provenance)
        .expect("declares");
    drop(store);
    let (storage, disk) = Mirrored::open(&dir.0.join("log"));
    (
        Store::open(storage, SystemClock).expect("opens store"),
        disk,
    )
}

/// Sent without waiting, `SET`s to a plain-async table and the `GET`s
/// between them are answered in the order sent, each `GET` with what the
/// `SET` before it wrote.
#[test]
fn gets_sent_behind_sets_to_a_plain_async_table_read_what_they_wrote() {
    const ROUNDS: usize = 200;
    let dir = TempDir::new("async-pipelined");
    let (store, _disk) = store_with_a_cache(&dir);
    let (_committer, mut client) = serve_syncing_every(store, Duration::from_secs(3600));
    // A first write of the subject makes its key; the rest need none.
    assert_eq!(ask_line(&mut client, &["SET", "cache/top", "start"]), "+OK");
    let mut request = Vec::new();
    for n in 0..ROUNDS {
        request.extend(command(&["SET", "cache/top", &format!("v{n}")]));
        request.extend(command(&["GET", "cache/top"]));
    }

    client.write_all(&request).expect("sends");

    for n in 0..ROUNDS {
        assert_eq!(read_line(&mut client), "+OK");
        let value = format!("v{n}");
        assert_eq!(read_line(&mut client), format!("${}", value.len()));
        assert_eq!(read_line(&mut client), value);
    }
}

/// While other clients write a plain-async table without a pause, a put
/// still reaches the disk about as soon as the server was told to sync:
/// the threads that answer such puts do not keep the one that syncs
/// from its turn.
#[test]
fn a_busy_plain_async_table_is_still_synced_in_time() {
    let dir = TempDir::new("async-busy");
    let (store, disk) = store_with_a_cache(&dir);
    let (_committer, mut client) = serve_syncing_every(store, Duration::from_millis(100));
    let address = client.peer_addr().expect("has an address");
    let stop = Arc::new(AtomicBool::new(false));
    let busy: Vec<_> = (0..4)
        .map(|n| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut client = TcpStream::connect(address).expect("connects");
                while !stop.load(Ordering::Relaxed) {
                    let key = format!("cache/busy/{n}");
                    assert_eq!(ask_line(&mut client, &["SET", &key, "x"]), "+OK");
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(50));

    assert_eq!(ask_line(&mut client, &["SET", "cache/marker", "1"]), "+OK");
    let answered = Instant::now();
    let on_disk = || {
        text(
            disk.after_a_power_cut(&dir)
                .get(MAIN_BRANCH, b"cache/marker"),
        )
    };
    while on_disk().is_none() {
        assert!(
            answered.elapsed() < Duration::from_secs(5),
            "not on disk five seconds after it was answered"
        );
        thread::sleep(Duration::from_millis(20));
    }

    stop.store(true, Ordering::Relaxed);
    for client in busy {
        client.join().expect("the busy client ends");
    }
}
