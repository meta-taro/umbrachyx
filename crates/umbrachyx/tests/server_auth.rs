//! Who a connection to the server is: `AUTH` with a capability token names
//! the author of its writes and bounds them; without it, writes are named
//! unauthenticated, and `require_auth` refuses them.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};

use biscuit_auth::KeyPair;
use umbrachyx::capability::{self, Grant, Operation};
use umbrachyx::log::FileStorage;
use umbrachyx::server::Server;
use umbrachyx::store::{Committer, MAIN_BRANCH, Store, SystemClock};
use umbrachyx::tree::Bytes;

const BRANCH: &str = "agent/support";
const AGENT: &str = "agent:support";
const PURPOSE: &str = "support tickets";
const DAY: Duration = Duration::from_secs(86_400);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-auth-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn start(
    dir: &TempDir,
    configure: impl FnOnce(Server) -> Server,
) -> (Committer<FileStorage, SystemClock>, TcpStream) {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    let committer = Committer::start(store);
    let server = configure(
        Server::new(BRANCH, committer.writer(), committer.reader()).expect("serves a branch"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    thread::spawn(move || server.serve(listener));
    (committer, TcpStream::connect(address).expect("connects"))
}

fn token(root: &KeyPair, operations: &[Operation], subject: &str, prefix: &str) -> String {
    let token = capability::issue(
        root,
        &Grant {
            operations,
            key_prefix: prefix,
            columns: &["gems"],
            purpose: PURPOSE,
            expires: SystemTime::now() + DAY,
            subject,
            branch: BRANCH,
        },
    )
    .expect("issues");
    capability::to_text(&token).expect("prints")
}

fn agent_token(root: &KeyPair) -> String {
    token(root, &[Operation::Read, Operation::Write], AGENT, "player/")
}

fn command(args: &[&str]) -> Vec<u8> {
    let mut bytes = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        bytes.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    bytes
}

/// One line of a reply, without its `\r\n`.
fn ask(client: &mut TcpStream, args: &[&str]) -> String {
    client.write_all(&command(args)).expect("sends");
    let mut line = Vec::new();
    let mut byte = [0];
    while !line.ends_with(b"\r\n") {
        client.read_exact(&mut byte).expect("reads a line");
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).expect("a line is text")
}

#[test]
fn a_connection_that_authenticates_writes_as_the_tokens_holder() {
    let dir = TempDir::new("holder");
    let root = KeyPair::new();
    let (committer, mut client) = start(&dir, |server| server.public_key(root.public()));

    assert_eq!(ask(&mut client, &["AUTH", &agent_token(&root)]), "+OK");
    assert_eq!(ask(&mut client, &["SET", "player/7/gems", "30"]), "+OK");

    let store = committer.stop();
    assert_eq!(store.log(BRANCH)[0].provenance.who, AGENT);
}

#[test]
fn auth_takes_a_user_name_before_the_token_as_redis_does() {
    let dir = TempDir::new("user");
    let root = KeyPair::new();
    let (committer, mut client) = start(&dir, |server| server.public_key(root.public()));

    assert_eq!(
        ask(&mut client, &["AUTH", "anyone", &agent_token(&root)]),
        "+OK"
    );
    assert_eq!(ask(&mut client, &["SET", "player/7/gems", "30"]), "+OK");

    let store = committer.stop();
    assert_eq!(store.log(BRANCH)[0].provenance.who, AGENT);
}

#[test]
fn a_token_from_another_root_is_refused_and_the_connection_stays_unauthenticated() {
    let dir = TempDir::new("wrongpass");
    let root = KeyPair::new();
    let (committer, mut client) = start(&dir, |server| server.public_key(root.public()));

    let refused = ask(&mut client, &["AUTH", &agent_token(&KeyPair::new())]);
    assert!(refused.starts_with("-WRONGPASS "), "{refused}");
    assert_eq!(ask(&mut client, &["SET", "player/7/gems", "30"]), "+OK");

    let store = committer.stop();
    assert_eq!(store.log(BRANCH)[0].provenance.who, "unauthenticated");
}

#[test]
fn an_authenticated_write_outside_the_token_is_refused_and_not_written() {
    let dir = TempDir::new("scope");
    let root = KeyPair::new();
    let (committer, mut client) = start(&dir, |server| server.public_key(root.public()));
    assert_eq!(ask(&mut client, &["AUTH", &agent_token(&root)]), "+OK");

    let refused = ask(&mut client, &["SET", "guild/1/gems", "30"]);
    assert!(refused.starts_with("-NOPERM "), "{refused}");
    let wrong_column = ask(&mut client, &["SET", "player/7/item", "sword"]);
    assert!(wrong_column.starts_with("-NOPERM "), "{wrong_column}");

    let store = committer.stop();
    assert!(store.log(BRANCH).is_empty());
}

#[test]
fn a_token_that_approves_cannot_write_through_the_server() {
    let dir = TempDir::new("approver");
    let root = KeyPair::new();
    let approver = token(
        &root,
        &[Operation::Read, Operation::Approve],
        "person:lead",
        "player/",
    );
    let (committer, mut client) = start(&dir, |server| server.public_key(root.public()));
    assert_eq!(ask(&mut client, &["AUTH", &approver]), "+OK");

    let refused = ask(&mut client, &["SET", "player/7/gems", "30"]);
    assert!(refused.starts_with("-NOPERM "), "{refused}");

    let store = committer.stop();
    assert!(store.log(BRANCH).is_empty());
}

#[test]
fn an_unauthenticated_write_is_named_so() {
    let dir = TempDir::new("unauthenticated");
    let (committer, mut client) = start(&dir, |server| server);

    assert_eq!(ask(&mut client, &["SET", "player/7/gems", "30"]), "+OK");

    let store = committer.stop();
    let commit = store.log(BRANCH)[0];
    assert_eq!(commit.provenance.who, "unauthenticated");
    assert_eq!(commit.provenance.why, "SET");
}

#[test]
fn require_auth_refuses_unauthenticated_writes_but_not_reads() {
    let dir = TempDir::new("require");
    let root = KeyPair::new();
    let (committer, mut client) = start(&dir, |server| {
        server.public_key(root.public()).require_auth(true)
    });

    assert_eq!(
        ask(&mut client, &["SET", "player/7/gems", "30"]),
        "-NOAUTH Authentication required."
    );
    assert_eq!(ask(&mut client, &["GET", "player/7/gems"]), "$-1");
    assert_eq!(ask(&mut client, &["AUTH", &agent_token(&root)]), "+OK");
    assert_eq!(ask(&mut client, &["SET", "player/7/gems", "30"]), "+OK");

    let view = committer.reader().view();
    assert_eq!(
        view.get(BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"30"[..]))
    );
}

#[test]
fn auth_on_a_server_without_a_public_key_is_an_error() {
    let dir = TempDir::new("no-key");
    let (_committer, mut client) = start(&dir, |server| server);

    let refused = ask(&mut client, &["AUTH", "anything"]);

    assert!(refused.starts_with("-ERR "), "{refused}");
    assert!(refused.contains("--public-key"), "{refused}");
}
