//! The server writing tables that keep no history: a `SET` to such a
//! table is a put, answered once durable like any other, read back by
//! `GET`, and it makes no commit on the branch the server serves.
//!
//! The data is synthetic: players and their sessions.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;

use umbrachyx::log::{Engine, FileStorage};
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
    let committer = Committer::start(store);
    let server =
        Server::new(BRANCH, committer.writer(), committer.reader()).expect("serves a branch");
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
