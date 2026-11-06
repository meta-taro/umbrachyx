//! The Redis-protocol server over a real socket: commands, pipelining, the
//! branch its writes go to, and what it refuses.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use umbrachyx::log::{FileStorage, Storage};
use umbrachyx::server::{Server, ServerError};
use umbrachyx::store::{Committer, MAIN_BRANCH, Store, SystemClock};
use umbrachyx::tree::Bytes;

const BRANCH: &str = "bench";
/// Long enough that writes pile up behind each sync on any machine.
const SLOW_SYNC: Duration = Duration::from_millis(5);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-server-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server on a free port of this machine, serving `BRANCH`.
fn start(dir: &TempDir) -> (Committer<FileStorage, SystemClock>, TcpStream) {
    let (committer, address) = start_with(dir, |server| server);
    let client = TcpStream::connect(address).expect("connects");
    (committer, client)
}

fn start_with(
    dir: &TempDir,
    configure: impl FnOnce(Server) -> Server,
) -> (Committer<FileStorage, SystemClock>, SocketAddr) {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    serve_store(storage, configure)
}

fn serve_store<S: Storage + Send + 'static>(
    storage: S,
    configure: impl FnOnce(Server) -> Server,
) -> (Committer<S, SystemClock>, SocketAddr) {
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    let committer = Committer::start(store);
    let server =
        Server::new(BRANCH, committer.writer(), committer.reader()).expect("serves a branch");
    let server = configure(server);
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    thread::spawn(move || server.serve(listener));
    (committer, address)
}

fn command(args: &[&str]) -> Vec<u8> {
    let mut bytes = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        bytes.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    bytes
}

/// Reads exactly as many bytes as `expected`, so a wrong reply fails the
/// test by its content.
fn read_reply(client: &mut TcpStream, expected: &[u8]) -> Vec<u8> {
    let mut reply = vec![0; expected.len()];
    client.read_exact(&mut reply).expect("reads the reply");
    reply
}

#[test]
fn pipelined_commands_are_answered_in_order() {
    let dir = TempDir::new("pipelined");
    let (_committer, mut client) = start(&dir);

    let mut requests = Vec::new();
    for args in [
        &["PING"][..],
        &["set", "player/1/gold", "100"],
        &["GET", "player/1/gold"],
        &["GET", "player/2/gold"],
        &["PING", "hello"],
    ] {
        requests.extend(command(args));
    }
    client.write_all(&requests).expect("sends");

    let expected = b"+PONG\r\n+OK\r\n$3\r\n100\r\n$-1\r\n$5\r\nhello\r\n";
    assert_eq!(read_reply(&mut client, expected), expected);
}

/// The file, with every sync counted and slowed down.
struct CountingStorage {
    file: FileStorage,
    syncs: Arc<AtomicUsize>,
}

impl Storage for CountingStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.append(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        thread::sleep(SLOW_SYNC);
        self.syncs.fetch_add(1, Ordering::SeqCst);
        self.file.sync()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.truncate(len)
    }
}

/// SETs sent together share syncs rather than waiting for one each, and
/// are answered in order; a command after them sees them all.
#[test]
fn pipelined_sets_share_syncs_and_later_commands_see_them() {
    const SETS: usize = 40;
    let dir = TempDir::new("pipelined-sets");
    let syncs = Arc::new(AtomicUsize::new(0));
    let storage = CountingStorage {
        file: FileStorage::open(&dir.0.join("log")).expect("opens file"),
        syncs: Arc::clone(&syncs),
    };
    let (_committer, address) = serve_store(storage, |server| server);
    let mut client = TcpStream::connect(address).expect("connects");
    let opening_syncs = syncs.load(Ordering::SeqCst);

    let mut requests = Vec::new();
    for n in 0..SETS {
        requests.extend(command(&["SET", "player/1/gold", &n.to_string()]));
    }
    requests.extend(command(&["GET", "player/1/gold"]));
    requests.extend(command(&["SET", "player/1/gold", "done"]));
    requests.extend(command(&["GET", "player/1/gold"]));
    client.write_all(&requests).expect("sends");

    let last = (SETS - 1).to_string();
    let mut expected = b"+OK\r\n".repeat(SETS);
    expected.extend(format!("${}\r\n{last}\r\n", last.len()).as_bytes());
    expected.extend(b"+OK\r\n$4\r\ndone\r\n");
    assert_eq!(read_reply(&mut client, &expected), expected);
    let used = syncs.load(Ordering::SeqCst) - opening_syncs;
    assert!(used * 4 <= SETS, "{used} syncs for {SETS} SETs");
}

/// Writes reach the server's branch, never main, with the client named as
/// their author.
#[test]
fn writes_go_to_the_branch_with_the_client_as_author() {
    let dir = TempDir::new("branch");
    let (committer, mut client) = start(&dir);

    client
        .write_all(&command(&["SET", "player/1/gold", "100"]))
        .expect("sends");
    assert_eq!(read_reply(&mut client, b"+OK\r\n"), b"+OK\r\n");

    let view = committer.reader().view();
    assert_eq!(
        view.get(BRANCH, b"player/1/gold"),
        Some(Bytes::from(&b"100"[..]))
    );
    assert_eq!(view.get(MAIN_BRANCH, b"player/1/gold"), None);
    let store = committer.stop();
    let latest = store.log(BRANCH)[0];
    let local = client.local_addr().expect("has an address");
    assert_eq!(latest.provenance.who, format!("resp {local}"));
    assert_eq!(latest.provenance.why, "SET");
}

#[test]
fn serving_main_or_a_missing_branch_is_refused() {
    let dir = TempDir::new("main");
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let store = Store::open(storage, SystemClock).expect("opens store");
    let committer = Committer::start(store);

    let refused = Server::new(MAIN_BRANCH, committer.writer(), committer.reader());
    assert!(matches!(refused, Err(ServerError::Main)));
    let missing = Server::new("missing", committer.writer(), committer.reader());
    assert!(matches!(missing, Err(ServerError::UnknownBranch(name)) if name == "missing"));
}

#[test]
fn unknown_commands_and_wrong_arguments_are_errors_and_the_connection_goes_on() {
    let dir = TempDir::new("errors");
    let (_committer, mut client) = start(&dir);

    let mut requests = Vec::new();
    for args in [
        &["FLUSHALL"][..],
        &["GET"],
        &["SET", "a", "1", "EX", "10"],
        &["PING"],
    ] {
        requests.extend(command(args));
    }
    client.write_all(&requests).expect("sends");

    let expected = b"-ERR unknown command 'FLUSHALL'\r\n\
-ERR wrong number of arguments for 'get' command\r\n\
-ERR SET options are not supported\r\n\
+PONG\r\n";
    assert_eq!(read_reply(&mut client, expected), expected);
}

#[test]
fn a_protocol_error_is_answered_and_closes_the_connection() {
    let dir = TempDir::new("protocol");
    let (_committer, mut client) = start(&dir);

    client.write_all(b"GET a\r\n").expect("sends");
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).expect("reads until closed");
    assert_eq!(reply, b"-ERR Protocol error: expected '*', got 'G'\r\n");
    let _ = client.shutdown(Shutdown::Both);
}

/// One connection past the limit is told so and closed; once a connection
/// ends, its place can be taken again.
#[test]
fn connections_past_the_limit_are_turned_away() {
    let dir = TempDir::new("limit");
    let (_committer, address) = start_with(&dir, |server| server.max_connections(1));
    let ping = command(&["PING"]);
    let mut first = TcpStream::connect(address).expect("connects");
    first.write_all(&ping).expect("sends");
    assert_eq!(read_reply(&mut first, b"+PONG\r\n"), b"+PONG\r\n");

    let mut second = TcpStream::connect(address).expect("connects");
    let mut reply = Vec::new();
    second.read_to_end(&mut reply).expect("reads until closed");
    assert_eq!(reply, b"-ERR max number of clients reached\r\n");

    first.shutdown(Shutdown::Both).expect("closes");
    let third = (0..100)
        .find_map(|_| {
            let mut client = TcpStream::connect(address).expect("connects");
            client.write_all(&ping).ok()?;
            let mut reply = vec![0; 7];
            client.read_exact(&mut reply).ok()?;
            (reply == b"+PONG\r\n").then_some(client)
        })
        .expect("a place opens once the first connection ends");
    third.shutdown(Shutdown::Both).expect("closes");
}

/// A command that arrives a byte at a time is answered once it is whole.
#[test]
fn a_command_sent_a_byte_at_a_time_is_answered() {
    let dir = TempDir::new("trickle");
    let (_committer, mut client) = start(&dir);
    client.set_nodelay(true).expect("sets nodelay");

    for byte in command(&["SET", "player/1/gold", "100"]) {
        client.write_all(&[byte]).expect("sends");
    }
    assert_eq!(read_reply(&mut client, b"+OK\r\n"), b"+OK\r\n");
}

/// Once one client is told a write succeeded, another reads it.
#[test]
fn a_write_acknowledged_to_one_client_is_read_by_another() {
    let dir = TempDir::new("two-clients");
    let (_committer, mut writer) = start(&dir);
    let address = writer.peer_addr().expect("connected");
    let mut reader = TcpStream::connect(address).expect("connects");

    for n in 0..20 {
        let value = n.to_string();
        writer
            .write_all(&command(&["SET", "player/1/gold", &value]))
            .expect("sends");
        assert_eq!(read_reply(&mut writer, b"+OK\r\n"), b"+OK\r\n");
        reader
            .write_all(&command(&["GET", "player/1/gold"]))
            .expect("sends");
        let expected = format!("${}\r\n{value}\r\n", value.len());
        let reply = read_reply(&mut reader, expected.as_bytes());
        assert_eq!(reply, expected.as_bytes());
    }
}

/// Commands before a malformed one in the same batch are carried out and
/// answered; the connection then closes.
#[test]
fn commands_before_a_protocol_error_still_count() {
    let dir = TempDir::new("partial-batch");
    let (committer, mut client) = start(&dir);

    let mut requests = command(&["SET", "player/1/gold", "100"]);
    requests.extend_from_slice(b"*1\r\n$3\r\nPINGX\r\n");
    client.write_all(&requests).expect("sends");

    let mut reply = Vec::new();
    client.read_to_end(&mut reply).expect("reads until closed");
    assert_eq!(reply, b"+OK\r\n-ERR Protocol error: expected CRLF\r\n");
    assert_eq!(
        committer.reader().view().get(BRANCH, b"player/1/gold"),
        Some(Bytes::from(&b"100"[..]))
    );
}

/// Sends one command and reads its reply as JSON in a bulk string.
fn ask_json(client: &mut TcpStream, args: &[&str]) -> serde_json::Value {
    client.write_all(&command(args)).expect("sends");
    let line = read_line(client);
    let len: usize = line
        .strip_prefix('$')
        .and_then(|len| len.parse().ok())
        .unwrap_or_else(|| panic!("{args:?} answers a bulk string, not {line:?}"));
    let mut body = vec![0; len + 2];
    client.read_exact(&mut body).expect("reads the body");
    serde_json::from_slice(&body[..len]).expect("the body is JSON")
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

fn set_all(client: &mut TcpStream, pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        client
            .write_all(&command(&["SET", key, value]))
            .expect("sends");
        assert_eq!(read_reply(client, b"+OK\r\n"), b"+OK\r\n");
    }
}

#[test]
fn history_shows_every_change_to_a_key_newest_first() {
    let dir = TempDir::new("history");
    let (_committer, mut client) = start(&dir);
    set_all(&mut client, &[("gold", "1"), ("gems", "5"), ("gold", "2")]);

    let history = ask_json(&mut client, &["UMBRA.HISTORY", BRANCH, "gold"]);

    let changes = history["changes"].as_array().expect("a list of changes");
    let values: Vec<_> = changes.iter().map(|change| &change["value"]).collect();
    assert_eq!(values, ["2", "1"]);
    assert_eq!(changes[0]["why"], "SET");
    assert_eq!(changes[0]["erased"], false);
    let who = changes[0]["who"].as_str().expect("who is text");
    assert!(who.starts_with("resp 127.0.0.1:"), "{who}");
    assert_ne!(changes[0]["commit"], changes[1]["commit"]);
}

#[test]
fn cells_list_live_keys_with_their_last_change_in_pages() {
    let dir = TempDir::new("cells");
    let (_committer, mut client) = start(&dir);
    set_all(
        &mut client,
        &[("b", "2"), ("a", "1"), ("c", "3"), ("a", "4")],
    );

    let all = ask_json(&mut client, &["UMBRA.CELLS", BRANCH]);
    let after_a = ask_json(&mut client, &["UMBRA.CELLS", BRANCH, "a"]);

    let keys = |page: &serde_json::Value| -> Vec<String> {
        page["cells"]
            .as_array()
            .expect("a list of cells")
            .iter()
            .map(|cell| cell["key"].as_str().expect("key").to_owned())
            .collect()
    };
    assert_eq!(keys(&all), ["a", "b", "c"]);
    assert_eq!(keys(&after_a), ["b", "c"]);
    assert_eq!(all["cells"][0]["value"], "4");
    assert_eq!(all["cells"][0]["why"], "SET");
    assert!(all["next"].is_null(), "everything fit in one page");
}

#[test]
fn branches_log_and_diff_show_what_the_branch_changed() {
    let dir = TempDir::new("log-diff");
    let (_committer, mut client) = start(&dir);
    set_all(&mut client, &[("gold", "1"), ("gold", "2")]);

    let branches = ask_json(&mut client, &["UMBRA.BRANCHES"]);
    let log = ask_json(&mut client, &["UMBRA.LOG", BRANCH]);
    let diff = ask_json(&mut client, &["UMBRA.DIFF", MAIN_BRANCH, BRANCH]);

    let names: Vec<_> = branches["branches"]
        .as_array()
        .expect("a list of branches")
        .iter()
        .map(|branch| branch["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, [BRANCH, MAIN_BRANCH]);
    assert_eq!(
        branches["served"], BRANCH,
        "a client learns which branch it writes to"
    );
    let commits = log["commits"].as_array().expect("a list of commits");
    assert_eq!(commits.len(), 2);
    assert_eq!(branches["branches"][0]["head"], commits[0]["commit"]);
    assert_eq!(commits[0]["parent"], commits[1]["commit"]);
    assert_eq!(log["more"], false);
    assert_eq!(
        diff["changes"],
        serde_json::json!([{"key": "gold", "before": null, "after": "2", "erased": false}])
    );
    assert_eq!(diff["more"], false);
}

#[test]
fn history_commands_refuse_unknown_branches_and_wrong_arguments() {
    let dir = TempDir::new("history-refused");
    let (_committer, mut client) = start(&dir);

    client
        .write_all(&command(&["UMBRA.HISTORY", "missing", "gold"]))
        .expect("sends");
    assert_eq!(read_line(&mut client), "-ERR no branch named \"missing\"");
    client
        .write_all(&command(&["UMBRA.DIFF", MAIN_BRANCH, "nowhere"]))
        .expect("sends");
    assert!(read_line(&mut client).starts_with("-ERR "));
    client
        .write_all(&command(&["UMBRA.HISTORY", BRANCH]))
        .expect("sends");
    assert_eq!(
        read_line(&mut client),
        "-ERR wrong number of arguments for 'umbra.history' command"
    );
}

/// A branch with more cells than one page answers with where to go on, and
/// the pages together hold every key once.
#[test]
fn cells_past_a_page_say_where_the_next_page_starts() {
    let dir = TempDir::new("cells-pages");
    let (_committer, mut client) = start(&dir);
    let total = umbrachyx::server::MAX_PAGE + 1;
    let sets: Vec<u8> = (0..total)
        .flat_map(|n| command(&["SET", &format!("k/{n:05}"), "v"]))
        .collect();
    client.write_all(&sets).expect("sends");
    let all_ok = b"+OK\r\n".repeat(total);
    assert_eq!(read_reply(&mut client, &all_ok), all_ok);

    let first = ask_json(&mut client, &["UMBRA.CELLS", BRANCH]);
    let next = first["next"].as_str().expect("more follow").to_owned();
    let second = ask_json(&mut client, &["UMBRA.CELLS", BRANCH, &next]);

    let count = |page: &serde_json::Value| page["cells"].as_array().expect("cells").len();
    assert_eq!(count(&first), umbrachyx::server::MAX_PAGE);
    assert_eq!(next, format!("k/{:05}", umbrachyx::server::MAX_PAGE - 1));
    assert_eq!(count(&second), 1);
    assert_eq!(second["cells"][0]["key"], format!("k/{:05}", total - 1));
    assert!(second["next"].is_null());
}

/// A key changed more often than a page holds answers with the newest
/// changes and says older ones were left out; so does a long log.
#[test]
fn long_histories_and_logs_answer_one_page() {
    let dir = TempDir::new("history-pages");
    let (_committer, mut client) = start(&dir);
    let total = umbrachyx::server::MAX_PAGE + 1;
    let sets: Vec<u8> = (0..total)
        .flat_map(|n| command(&["SET", "gold", &n.to_string()]))
        .collect();
    client.write_all(&sets).expect("sends");
    let all_ok = b"+OK\r\n".repeat(total);
    assert_eq!(read_reply(&mut client, &all_ok), all_ok);

    let history = ask_json(&mut client, &["UMBRA.HISTORY", BRANCH, "gold"]);
    let log = ask_json(&mut client, &["UMBRA.LOG", BRANCH]);

    let changes = history["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), umbrachyx::server::MAX_PAGE);
    assert_eq!(changes[0]["value"], (total - 1).to_string());
    assert_eq!(history["more"], true);
    let commits = log["commits"].as_array().expect("commits");
    assert_eq!(commits.len(), umbrachyx::server::MAX_PAGE);
    assert_eq!(log["more"], true);
}

/// A diff over more keys than a page holds answers with a page and says
/// the rest were left out.
#[test]
fn a_long_diff_answers_one_page() {
    let dir = TempDir::new("diff-pages");
    let (_committer, mut client) = start(&dir);
    let total = umbrachyx::server::MAX_PAGE + 1;
    let sets: Vec<u8> = (0..total)
        .flat_map(|n| command(&["SET", &format!("k/{n:05}"), "v"]))
        .collect();
    client.write_all(&sets).expect("sends");
    let all_ok = b"+OK\r\n".repeat(total);
    assert_eq!(read_reply(&mut client, &all_ok), all_ok);

    let diff = ask_json(&mut client, &["UMBRA.DIFF", MAIN_BRANCH, BRANCH]);

    let changes = diff["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), umbrachyx::server::MAX_PAGE);
    assert_eq!(changes[0]["key"], "k/00000");
    assert_eq!(diff["more"], true);
}

/// What a viewer sends to list keys and open one: `SCAN` walks the served
/// branch in key order a page at a time, and `MATCH` keeps the keys that fit.
#[test]
fn scan_walks_the_served_branch_in_pages() {
    let dir = TempDir::new("scan");
    let (_committer, mut client) = start(&dir);
    set_all(
        &mut client,
        &[("b/1", "x"), ("a/2", "x"), ("a/1", "x"), ("c", "x")],
    );

    let mut requests = Vec::new();
    for args in [
        &["SCAN", "0", "COUNT", "2"][..],
        &["SCAN", "2", "COUNT", "10"],
        &["scan", "0", "MATCH", "a/*"],
        &["SCAN", "0", "COUNT", "3", "MATCH", "b/*"],
    ] {
        requests.extend(command(args));
    }
    client.write_all(&requests).expect("sends");

    let expected = b"*2\r\n$1\r\n2\r\n*2\r\n$3\r\na/1\r\n$3\r\na/2\r\n\
*2\r\n$1\r\n0\r\n*2\r\n$3\r\nb/1\r\n$1\r\nc\r\n\
*2\r\n$1\r\n0\r\n*2\r\n$3\r\na/1\r\n$3\r\na/2\r\n\
*2\r\n$1\r\n3\r\n*1\r\n$3\r\nb/1\r\n";
    assert_eq!(
        String::from_utf8_lossy(&read_reply(&mut client, expected)),
        String::from_utf8_lossy(expected)
    );
}

/// Every value is a string and none expires, so `TYPE` and `TTL` answer as
/// Redis does for such a key, and as for a missing one otherwise.
#[test]
fn type_and_ttl_answer_for_strings_that_never_expire() {
    let dir = TempDir::new("type-ttl");
    let (_committer, mut client) = start(&dir);
    set_all(&mut client, &[("gold", "1")]);

    let mut requests = Vec::new();
    for args in [
        &["TYPE", "gold"][..],
        &["TYPE", "missing"],
        &["TTL", "gold"],
        &["TTL", "missing"],
    ] {
        requests.extend(command(args));
    }
    client.write_all(&requests).expect("sends");

    let expected = b"+string\r\n+none\r\n:-1\r\n:-2\r\n";
    assert_eq!(read_reply(&mut client, expected), expected);
}

#[test]
fn scan_refuses_what_it_does_not_take() {
    let dir = TempDir::new("scan-refused");
    let (_committer, mut client) = start(&dir);

    let mut requests = Vec::new();
    for args in [
        &["SCAN", "x"][..],
        &["SCAN", "0", "COUNT", "0"],
        &["SCAN", "0", "TYPE", "hash"],
        &["SCAN"],
        &["TYPE"],
    ] {
        requests.extend(command(args));
    }
    client.write_all(&requests).expect("sends");

    let expected = b"-ERR invalid cursor\r\n\
-ERR syntax error\r\n\
-ERR syntax error\r\n\
-ERR wrong number of arguments for 'scan' command\r\n\
-ERR wrong number of arguments for 'type' command\r\n";
    assert_eq!(
        String::from_utf8_lossy(&read_reply(&mut client, expected)),
        String::from_utf8_lossy(expected)
    );
}
