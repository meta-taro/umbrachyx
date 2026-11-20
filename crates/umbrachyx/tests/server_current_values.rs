//! A replica started to hold current values only: it answers reads of
//! values as any replica does, and says of every question about the
//! past that it keeps none, instead of answering that nothing happened.
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
const LIMIT: Duration = Duration::from_secs(30);
const AGAIN: Duration = Duration::from_millis(5);

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-current-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn free_address() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    (listener, address)
}

/// A primary with one player, and a replica of it that holds current
/// values only.
fn pair(dir: &TempDir) -> (Client, Client) {
    let mut store = Store::open(
        FileStorage::open(&dir.0.join("primary")).expect("opens file"),
        SystemClock,
    )
    .expect("opens store");
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    let edit = Edit {
        key: b"player/1/gems".to_vec(),
        value: Some(b"100".to_vec()),
    };
    let provenance = Provenance {
        who: "seed".to_owned(),
        why: "test data".to_owned(),
        approved_by: None,
    };
    store
        .commit(BRANCH, vec![edit], provenance)
        .expect("commits");
    let replica = copy_of(dir, &mut store);
    let committer = Committer::start(store);
    let (listener, primary) = free_address();
    let (feed, feed_address) = free_address();
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .feed_replicas_on(feed);
    thread::spawn(move || {
        let _committer = committer;
        server.serve(listener)
    });
    let committer = Committer::start_following(replica);
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .follow(feed_address.to_string());
    let (listener, replica) = free_address();
    thread::spawn(move || {
        let _committer = committer;
        server.serve(listener)
    });
    (Client::connect(primary), Client::connect(replica))
}

/// A backup of `from` restored as another store, opened to hold current
/// values only.
fn copy_of(dir: &TempDir, from: &mut FileStore) -> FileStore {
    let backup = dir.0.join("backup");
    let mut target = DirTarget::open(&backup).expect("opens the backup");
    from.backup(&mut target).expect("backs up");
    drop(target);
    let path: &Path = &dir.0.join("replica");
    let mut log = FileStorage::open(path).expect("opens file");
    restore(&mut DirSource::new(&backup), &mut log, None).expect("restores");
    drop(log);
    Store::open_current_values(
        FileStorage::open(path).expect("opens file"),
        None,
        SystemClock,
    )
    .expect("opens store")
}

struct Client(BufReader<TcpStream>);

impl Client {
    fn connect(address: SocketAddr) -> Self {
        Self(BufReader::new(
            TcpStream::connect(address).expect("connects"),
        ))
    }

    /// Sends a command and returns its reply: a value's bytes, or the
    /// line of a status, an error or an array with the sign it starts
    /// with.
    fn text(&mut self, command: &[&str]) -> String {
        let mut out = format!("*{}\r\n", command.len());
        for arg in command {
            out.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
        }
        self.0.get_mut().write_all(out.as_bytes()).expect("sends");
        let mut line = String::new();
        self.0.read_line(&mut line).expect("reads");
        let line = line.trim_end();
        let Some(len) = line.strip_prefix('$') else {
            return line.to_owned();
        };
        if len == "-1" {
            return "(nil)".to_owned();
        }
        let mut value = vec![0; len.parse::<usize>().expect("a length") + 2];
        self.0.read_exact(&mut value).expect("reads");
        value.truncate(value.len() - 2);
        String::from_utf8(value).expect("is text")
    }

    /// Sends a command and returns everything that comes back in the
    /// next moments, as text. Nothing else can be asked on this
    /// connection afterwards.
    fn all_of(&mut self, command: &[&str]) -> String {
        let mut out = format!("*{}\r\n", command.len());
        for arg in command {
            out.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
        }
        let stream = self.0.get_mut();
        stream.write_all(out.as_bytes()).expect("sends");
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("sets a timeout");
        let mut reply = Vec::new();
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = self.0.read(&mut chunk) {
            reply.extend_from_slice(&chunk[..read]);
        }
        String::from_utf8_lossy(&reply).into_owned()
    }

    fn json(&mut self, command: &[&str]) -> serde_json::Value {
        serde_json::from_str(&self.text(command)).expect("is JSON")
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
}

#[test]
fn it_answers_reads_of_values_as_any_replica_does() {
    let dir = TempDir::new("reads");
    let (mut primary, mut replica) = pair(&dir);
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");

    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    assert_eq!(primary.text(&["SET", "player/2/gems", "7"]), "+OK");
    assert_eq!(primary.text(&["SET", "config", "v1"]), "+OK");

    replica.wait_for(&["GET", "config"], "v1");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "150");
    assert_eq!(replica.text(&["GET", "player/2/gems"]), "7");
    assert_eq!(replica.text(&["GET", "player/9/gems"]), "(nil)");
    let refused = replica.text(&["SET", "player/1/gems", "1"]);
    assert!(refused.starts_with("-READONLY "), "{refused}");
    // Last on this connection: the whole reply, whatever its shape.
    let listed = replica.all_of(&["SCAN", "0", "COUNT", "100"]);
    for key in ["player/1/gems", "player/2/gems", "config"] {
        assert!(listed.contains(key), "{key} is not in {listed:?}");
    }
}

#[test]
fn it_says_of_every_question_about_the_past_that_it_keeps_none() {
    let dir = TempDir::new("past");
    let (mut primary, mut replica) = pair(&dir);
    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    replica.wait_for(&["GET", "player/1/gems"], "150");
    // What the primary answers, so that a refusal below is the role's
    // and not the command's.
    assert!(
        primary
            .text(&["UMBRA.HISTORY", BRANCH, "player/1/gems"])
            .contains("150")
    );
    let zeros = "0".repeat(64);
    let questions: [&[&str]; 6] = [
        &["UMBRA.HISTORY", BRANCH, "player/1/gems"],
        &["UMBRA.LOG", BRANCH],
        &["UMBRA.CELLS", BRANCH],
        &["UMBRA.DIFF", MAIN_BRANCH, BRANCH],
        &["UMBRA.PENDING"],
        &["UMBRA.DECISIONS"],
    ];

    for question in questions {
        let said = replica.text(question);

        assert!(
            said.starts_with("-NOHISTORY this server keeps current values only"),
            "{question:?}: {said}"
        );
    }
    let unknown = replica.text(&["UMBRA.HISTORY", "no-such-branch", &zeros]);
    assert!(unknown.starts_with("-NOHISTORY "), "{unknown}");
}

#[test]
fn it_says_what_it_holds_and_which_branches_there_are() {
    let dir = TempDir::new("says");
    let (mut primary, mut replica) = pair(&dir);
    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    replica.wait_for(&["GET", "player/1/gems"], "150");

    let replication = replica.json(&["UMBRA.REPLICATION"]);
    let tables = replica.json(&["UMBRA.TABLES"]);
    let branches = replica.json(&["UMBRA.BRANCHES"]);
    let on_primary = primary.json(&["UMBRA.BRANCHES"]);

    assert_eq!(replication["holds"], "current values", "{replication}");
    assert_eq!(primary.json(&["UMBRA.REPLICATION"])["holds"], "history");
    assert_eq!(tables["keeps_history"], false, "{tables}");
    assert_eq!(primary.json(&["UMBRA.TABLES"])["keeps_history"], true);
    let head = |said: &serde_json::Value| {
        let branches = said["branches"].as_array().expect("lists branches").clone();
        let served = branches
            .into_iter()
            .find(|branch| branch["name"] == BRANCH)
            .expect("has the branch served");
        served["head"].clone()
    };
    // The same commit, though it keeps no record of it.
    assert_eq!(head(&branches), head(&on_primary));
    assert!(head(&branches).is_string(), "{branches}");
}
