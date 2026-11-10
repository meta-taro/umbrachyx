//! What the server's `UMBRA.*` commands say about tables held by different
//! engines: which tables are declared, that a latest-value table has no
//! history, and which tables a listing or a diff leaves out.
//!
//! The data is synthetic: players and their positions.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;

use serde_json::{Value, json};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::server::Server;
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};

const BRANCH: &str = "bench";

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-tables-{}-{name}",
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

fn by(why: &str) -> Provenance {
    Provenance {
        who: "test".to_owned(),
        why: why.to_owned(),
        approved_by: None,
    }
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

/// A store with `pos` declared latest, and `BRANCH` made from main.
fn store(dir: &TempDir) -> FileStore {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store
        .declare_table(b"pos", Engine::Latest, by("positions change every tick"))
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

fn set_all(client: &mut TcpStream, pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        assert_eq!(ask_line(client, &["SET", key, value]), "+OK");
    }
}

#[test]
fn tables_lists_the_declared_tables_and_the_default() {
    let dir = TempDir::new("tables");
    let (_committer, mut client) = serve(store(&dir));

    let tables = ask_json(&mut client, &["UMBRA.TABLES"]);

    assert_eq!(
        tables,
        json!({"default": "history", "tables": [{"name": "pos", "engine": "latest"}]})
    );
    assert_eq!(
        ask_line(&mut client, &["UMBRA.TABLES", "pos"]),
        "-ERR wrong number of arguments for 'umbra.tables' command"
    );
}

#[test]
fn tables_is_empty_when_nothing_is_declared() {
    let dir = TempDir::new("no-tables");
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    let (_committer, mut client) = serve(store);

    assert_eq!(
        ask_json(&mut client, &["UMBRA.TABLES"]),
        json!({"default": "history", "tables": []})
    );
    let cells = ask_json(&mut client, &["UMBRA.CELLS", BRANCH]);
    assert_eq!(cells["omitted_tables"], json!([]));
}

/// The first word of the error names its kind, as `WRONGTYPE` does, so a
/// tool can tell "this key keeps no history" from a failure.
#[test]
fn history_of_a_latest_key_answers_nohistory() {
    let dir = TempDir::new("nohistory");
    let (_committer, mut client) = serve(store(&dir));
    set_all(&mut client, &[("pos/1", "3,4"), ("player/1/gold", "7")]);

    assert_eq!(
        ask_line(&mut client, &["UMBRA.HISTORY", BRANCH, "pos/1"]),
        "-NOHISTORY table 'pos' keeps only the latest value"
    );
    let gold = ask_json(&mut client, &["UMBRA.HISTORY", BRANCH, "player/1/gold"]);
    assert_eq!(gold["changes"][0]["value"], "7");
}

#[test]
fn cells_leave_latest_tables_out_and_name_them() {
    let dir = TempDir::new("cells");
    let (_committer, mut client) = serve(store(&dir));
    set_all(&mut client, &[("pos/1", "3,4"), ("player/1/gold", "7")]);

    let cells = ask_json(&mut client, &["UMBRA.CELLS", BRANCH]);

    let keys: Vec<&Value> = cells["cells"]
        .as_array()
        .expect("a list of cells")
        .iter()
        .map(|cell| &cell["key"])
        .collect();
    assert_eq!(keys, ["player/1/gold"]);
    assert_eq!(cells["omitted_tables"], json!(["pos"]));
    assert_eq!(
        ask_line(&mut client, &["GET", "pos/1"]),
        "$3",
        "the current value is still read with GET"
    );
}

#[test]
fn a_diff_between_branch_heads_is_whole() {
    let dir = TempDir::new("diff-heads");
    let (_committer, mut client) = serve(store(&dir));
    set_all(&mut client, &[("pos/1", "3,4"), ("player/1/gold", "7")]);

    let diff = ask_json(&mut client, &["UMBRA.DIFF", MAIN_BRANCH, BRANCH]);

    let keys: Vec<&Value> = diff["changes"]
        .as_array()
        .expect("a list of changes")
        .iter()
        .map(|change| &change["key"])
        .collect();
    assert_eq!(keys, ["player/1/gold", "pos/1"]);
    assert_eq!(diff["omitted_tables"], json!([]));
}

/// Main keeps no past for a latest-value table, so a diff into it gives
/// what it can and names the table it left out.
#[test]
fn a_diff_into_the_past_of_main_names_the_tables_it_left_out() {
    let dir = TempDir::new("diff-past");
    let mut store = store(&dir);
    for (pos, gold) in [("1,1", "1"), ("2,2", "2")] {
        store
            .commit(
                MAIN_BRANCH,
                vec![set("pos/1", pos), set("player/1/gold", gold)],
                by("tick"),
            )
            .expect("commits");
    }
    let (_committer, mut client) = serve(store);

    let diff = ask_json(&mut client, &["UMBRA.DIFF", "main~1", MAIN_BRANCH]);

    assert_eq!(
        diff["changes"],
        json!([{"key": "player/1/gold", "before": "1", "after": "2", "erased": false}])
    );
    assert_eq!(diff["omitted_tables"], json!(["pos"]));
}
