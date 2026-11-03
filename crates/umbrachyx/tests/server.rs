//! The Redis-protocol server over a real socket: commands, pipelining, the
//! branch its writes go to, and what it refuses.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;

use umbrachyx::log::FileStorage;
use umbrachyx::server::{Server, ServerError};
use umbrachyx::store::{Committer, MAIN_BRANCH, Store, SystemClock};
use umbrachyx::tree::Bytes;

const BRANCH: &str = "bench";

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
