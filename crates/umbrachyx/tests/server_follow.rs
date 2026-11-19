//! A server that follows another: it answers reads with what its primary
//! was sent, takes no writes, says how far it is, and keeps answering
//! when its primary cannot be reached or will not feed it.
//!
//! The data is synthetic: players and their gems.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::server::Server;
use umbrachyx::store::backup::{DirSource, DirTarget, restore};
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};

const BRANCH: &str = "bench";
/// How long a replica gets to hold what its primary was sent. It takes
/// milliseconds; this only has to be longer than a busy machine makes it.
const LIMIT: Duration = Duration::from_secs(30);
const AGAIN: Duration = Duration::from_millis(5);

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-follow-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    /// A store with the branch served and one player.
    fn primary(&self) -> FileStore {
        let mut store = open(&self.0.join("primary"));
        store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
        commit(&mut store, "player/1/gems", "100");
        store
    }

    /// A backup of `from` restored as another store: how a replica
    /// starts.
    fn copy_of(&self, from: &mut FileStore) -> FileStore {
        let backup = self.0.join("backup");
        let mut target = DirTarget::open(&backup).expect("opens the backup");
        from.backup(&mut target).expect("backs up");
        let path = self.0.join("replica");
        let mut log = FileStorage::open(&path).expect("opens file");
        restore(&mut DirSource::new(&backup), &mut log, None).expect("restores");
        drop(log);
        open(&path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(path: &Path) -> FileStore {
    let storage = FileStorage::open(path).expect("opens file");
    Store::open(storage, SystemClock).expect("opens store")
}

fn commit(store: &mut FileStore, key: &str, value: &str) {
    let edit = Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    };
    let provenance = Provenance {
        who: "seed".to_owned(),
        why: "test data".to_owned(),
        approved_by: None,
    };
    store
        .commit(BRANCH, vec![edit], provenance)
        .expect("commits");
}

fn free_address() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    (listener, address)
}

/// Serves `store` as a primary on `listener`.
fn serve_primary(store: FileStore, listener: TcpListener, feeds: bool) {
    let committer = Committer::start(store);
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .feed_replicas(feeds);
    thread::spawn(move || {
        // Kept for as long as the server runs: stopping it would end
        // the store under the server.
        let _committer = committer;
        server.serve(listener)
    });
}

/// Serves `store` as a replica of the server at `primary`, and returns
/// where.
fn serve_replica(store: FileStore, primary: SocketAddr) -> SocketAddr {
    let committer = Committer::start_following(store);
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .follow(primary.to_string());
    let (listener, address) = free_address();
    thread::spawn(move || {
        let _committer = committer;
        server.serve(listener)
    });
    address
}

/// A primary that feeds replicas and one replica of it.
fn pair(dir: &TempDir) -> (Client, Client) {
    let mut store = dir.primary();
    let replica = dir.copy_of(&mut store);
    let (listener, primary) = free_address();
    serve_primary(store, listener, true);
    let replica = serve_replica(replica, primary);
    (Client::connect(primary), Client::connect(replica))
}

struct Client(BufReader<TcpStream>);

impl Client {
    fn connect(address: SocketAddr) -> Self {
        Self(BufReader::new(
            TcpStream::connect(address).expect("connects"),
        ))
    }

    /// Sends a command and returns its reply: a value's bytes, or the
    /// line of a status or an error with the sign it starts with.
    fn ask(&mut self, command: &[&str]) -> Vec<u8> {
        let mut out = format!("*{}\r\n", command.len());
        for arg in command {
            out.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
        }
        self.0.get_mut().write_all(out.as_bytes()).expect("sends");
        let mut line = String::new();
        self.0.read_line(&mut line).expect("reads");
        let line = line.trim_end();
        let Some(len) = line.strip_prefix('$') else {
            return line.as_bytes().to_vec();
        };
        if len == "-1" {
            return b"(nil)".to_vec();
        }
        let mut value = vec![0; len.parse::<usize>().expect("a length") + 2];
        self.0.read_exact(&mut value).expect("reads");
        value.truncate(value.len() - 2);
        value
    }

    fn text(&mut self, command: &[&str]) -> String {
        String::from_utf8(self.ask(command)).expect("is text")
    }

    fn replication(&mut self) -> serde_json::Value {
        serde_json::from_str(&self.text(&["UMBRA.REPLICATION"])).expect("is JSON")
    }

    /// Asks again until the reply to `command` is `wanted`.
    fn wait_for(&mut self, command: &[&str], wanted: &str) {
        let start = Instant::now();
        loop {
            let reply = self.text(command);
            if reply == wanted {
                return;
            }
            assert!(
                start.elapsed() < LIMIT,
                "{command:?} was not answered {wanted:?} within {LIMIT:?}, but {reply:?}"
            );
            thread::sleep(AGAIN);
        }
    }

    /// Asks again until what the server says of its replication passes
    /// `is_so`, and returns it.
    fn wait_for_replication(
        &mut self,
        is_so: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let start = Instant::now();
        loop {
            let said = self.replication();
            if is_so(&said) {
                return said;
            }
            assert!(start.elapsed() < LIMIT, "within {LIMIT:?} it said {said}");
            thread::sleep(AGAIN);
        }
    }
}

#[test]
fn a_replica_answers_reads_with_what_its_primary_was_sent() {
    let dir = TempDir::new("reads");
    let (mut primary, mut replica) = pair(&dir);
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");

    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    assert_eq!(primary.text(&["SET", "player/2/gems", "7"]), "+OK");

    replica.wait_for(&["GET", "player/2/gems"], "7");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "150");
    // The history came with the values, under the same ids.
    let history = ["UMBRA.HISTORY", BRANCH, "player/1/gems"];
    assert_eq!(replica.text(&history), primary.text(&history));
}

#[test]
fn a_replica_takes_no_writes() {
    let dir = TempDir::new("read-only");
    let (mut primary, mut replica) = pair(&dir);

    let refused = replica.text(&["SET", "player/1/gems", "999"]);

    assert!(refused.starts_with("-READONLY "), "{refused}");
    // It still follows: a record of its own would have ended that.
    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    replica.wait_for(&["GET", "player/1/gems"], "150");
}

#[test]
fn a_replica_says_how_far_it_is_and_a_primary_that_it_is_one() {
    let dir = TempDir::new("status");
    let (mut primary, mut replica) = pair(&dir);
    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    replica.wait_for(&["GET", "player/1/gems"], "150");

    let said = replica.wait_for_replication(|said| said["records"] == said["primary_records"]);

    assert_eq!(said["role"], "replica");
    assert!(said["records"].as_u64().expect("a number") >= 3, "{said}");
    assert!(said["stopped"].is_null(), "{said}");
    assert!(said["unreachable"].is_null(), "{said}");
    let said = primary.replication();
    assert_eq!(said["role"], "primary");
    assert_eq!(said["feeds_replicas"], true);
}

#[test]
fn a_server_hands_its_log_to_nobody_unless_it_was_started_to() {
    let dir = TempDir::new("not-feeding");
    let mut store = dir.primary();
    let replica = dir.copy_of(&mut store);
    let (listener, address) = free_address();
    serve_primary(store, listener, false);
    let mut primary = Client::connect(address);
    let zeros = "0".repeat(64);

    let refused = primary.text(&["UMBRA.LOG.AFTER", &zeros[..32], "0", &zeros, "1024"]);

    assert!(
        refused.starts_with("-ERR this server hands its log to no replica"),
        "{refused}"
    );
    // A replica of it says so, and answers with what it holds.
    let mut replica = Client::connect(serve_replica(replica, address));
    let said = replica.wait_for_replication(|said| !said["unreachable"].is_null());
    let why = said["unreachable"].as_str().expect("says why");
    assert!(why.contains("hands its log to no replica"), "{why}");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");
}

#[test]
fn a_replica_written_to_apart_stops_following_and_keeps_answering() {
    let dir = TempDir::new("apart");
    let mut store = dir.primary();
    let mut replica = dir.copy_of(&mut store);
    commit(&mut replica, "player/1/gems", "written here");
    commit(&mut store, "player/1/gems", "written there");
    commit(&mut store, "player/2/gems", "7");
    let (listener, address) = free_address();
    serve_primary(store, listener, true);

    let mut replica = Client::connect(serve_replica(replica, address));

    let said = replica.wait_for_replication(|said| !said["stopped"].is_null());
    let why = said["stopped"].as_str().expect("says why");
    assert!(why.contains("written to apart"), "{why}");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "written here");
    assert_eq!(replica.text(&["GET", "player/2/gems"]), "(nil)");
}

#[test]
fn a_replica_started_before_its_primary_follows_it_once_it_is_there() {
    let dir = TempDir::new("later");
    let mut store = dir.primary();
    let replica = dir.copy_of(&mut store);
    commit(&mut store, "player/1/gems", "150");
    // Nothing listens there yet.
    let (listener, address) = free_address();
    drop(listener);
    let mut replica = Client::connect(serve_replica(replica, address));
    replica.wait_for_replication(|said| !said["unreachable"].is_null());
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");

    let listener = TcpListener::bind(address).expect("the address is still free");
    serve_primary(store, listener, true);

    replica.wait_for(&["GET", "player/1/gems"], "150");
    let said = replica.wait_for_replication(|said| said["unreachable"].is_null());
    assert!(said["stopped"].is_null(), "{said}");
}
