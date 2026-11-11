//! A person approving or rejecting an agent's change over the server:
//! `UMBRA.PENDING` lists it, and `UMBRA.APPROVE` / `UMBRA.REJECT` act on
//! it only for a connection whose token may approve every key it touches.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};

use biscuit_auth::KeyPair;
use umbrachyx::capability::{self, Grant, Operation};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::server::Server;
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};
use umbrachyx::tree::Bytes;

const AGENT_BRANCH: &str = "agent/support";
const PURPOSE: &str = "support tickets";
const DAY: Duration = Duration::from_secs(86_400);

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-review-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn by(who: &str, why: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
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

/// A server for the agent's branch, which holds one change to main's data.
fn start(dir: &TempDir, root: &KeyPair) -> (Committer<FileStorage, SystemClock>, TcpStream) {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/7/gems", "30")],
            by("ops:seed", "seed"),
        )
        .expect("commits");
    store
        .create_branch(AGENT_BRANCH, MAIN_BRANCH)
        .expect("branches");
    store
        .commit(
            AGENT_BRANCH,
            vec![set("player/7/gems", "530")],
            by("agent:support", "ticket 8812"),
        )
        .expect("commits");
    let committer = Committer::start(store);
    let server = Server::new(AGENT_BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .public_key(root.public());
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
            branch: AGENT_BRANCH,
        },
    )
    .expect("issues");
    capability::to_text(&token).expect("prints")
}

fn approver(root: &KeyPair) -> String {
    token(
        root,
        &[Operation::Read, Operation::Approve],
        "person:lead",
        "player/",
    )
}

/// A RESP reply, read whole.
#[derive(Debug, PartialEq)]
enum Resp {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Option<String>),
    Array(Vec<Resp>),
}

fn read(reader: &mut BufReader<TcpStream>) -> Resp {
    let mut line = String::new();
    reader.read_line(&mut line).expect("reads a line");
    let line = line.trim_end_matches("\r\n");
    let (kind, rest) = line.split_at(1);
    match kind {
        "+" => Resp::Simple(rest.to_owned()),
        "-" => Resp::Error(rest.to_owned()),
        ":" => Resp::Integer(rest.parse().expect("an integer")),
        "$" => {
            let len: i64 = rest.parse().expect("a length");
            if len < 0 {
                return Resp::Bulk(None);
            }
            let mut bytes = vec![0; len as usize + 2];
            reader.read_exact(&mut bytes).expect("reads the value");
            bytes.truncate(len as usize);
            Resp::Bulk(Some(String::from_utf8(bytes).expect("text")))
        }
        "*" => {
            let len: usize = rest.parse().expect("a length");
            Resp::Array((0..len).map(|_| read(reader)).collect())
        }
        other => panic!("unknown reply kind {other:?}"),
    }
}

struct Client {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn new(stream: TcpStream) -> Self {
        let reader = BufReader::new(stream.try_clone().expect("clones"));
        Self {
            writer: stream,
            reader,
        }
    }

    fn ask(&mut self, args: &[&str]) -> Resp {
        let mut bytes = format!("*{}\r\n", args.len()).into_bytes();
        for arg in args {
            bytes.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
        }
        self.writer.write_all(&bytes).expect("sends");
        read(&mut self.reader)
    }
}

/// A field of one element of `UMBRA.PENDING`.
fn field<'a>(change: &'a Resp, name: &str) -> &'a Resp {
    let Resp::Array(items) = change else {
        panic!("a change is an array, got {change:?}");
    };
    let at = items
        .iter()
        .position(|item| *item == Resp::Bulk(Some(name.to_owned())))
        .unwrap_or_else(|| panic!("no field {name} in {change:?}"));
    &items[at + 1]
}

fn the_change(client: &mut Client) -> (String, String) {
    let Resp::Array(changes) = client.ask(&["UMBRA.PENDING"]) else {
        panic!("PENDING answers an array");
    };
    assert_eq!(changes.len(), 1, "{changes:?}");
    let Resp::Bulk(Some(branch)) = field(&changes[0], "branch") else {
        panic!("a branch name");
    };
    let Resp::Bulk(Some(head)) = field(&changes[0], "head") else {
        panic!("a head");
    };
    (branch.clone(), head.clone())
}

#[test]
fn pending_lists_each_change_as_one_flat_element() {
    let dir = TempDir::new("pending");
    let root = KeyPair::new();
    let (_committer, stream) = start(&dir, &root);
    let mut client = Client::new(stream);

    let Resp::Array(changes) = client.ask(&["UMBRA.PENDING"]) else {
        panic!("PENDING answers an array");
    };

    assert_eq!(changes.len(), 1);
    let change = &changes[0];
    assert_eq!(
        field(change, "branch"),
        &Resp::Bulk(Some(AGENT_BRANCH.to_owned()))
    );
    assert_eq!(
        field(change, "authors"),
        &Resp::Array(vec![Resp::Bulk(Some("agent:support".to_owned()))])
    );
    assert_eq!(
        field(change, "why"),
        &Resp::Bulk(Some("ticket 8812".to_owned()))
    );
    assert_eq!(
        field(change, "tables"),
        &Resp::Array(vec![
            Resp::Bulk(Some("player".to_owned())),
            Resp::Integer(1)
        ])
    );
    assert_eq!(field(change, "main_moved"), &Resp::Integer(0));
    assert_eq!(field(change, "conflicts"), &Resp::Integer(0));
    assert!(matches!(field(change, "first"), Resp::Integer(_)));
    assert!(matches!(field(change, "last"), Resp::Integer(_)));
}

#[test]
fn approving_needs_auth_and_a_token_that_may_approve() {
    let dir = TempDir::new("who");
    let root = KeyPair::new();
    let (committer, stream) = start(&dir, &root);
    let mut client = Client::new(stream);
    let (branch, head) = the_change(&mut client);

    let unauthenticated = client.ask(&["UMBRA.APPROVE", &branch, &head, "looks right"]);
    assert_eq!(
        unauthenticated,
        Resp::Error("NOAUTH Authentication required.".to_owned())
    );

    let agent = token(
        &root,
        &[Operation::Read, Operation::Write],
        "agent:support",
        "player/",
    );
    assert_eq!(client.ask(&["AUTH", &agent]), Resp::Simple("OK".to_owned()));
    let Resp::Error(refused) = client.ask(&["UMBRA.APPROVE", &branch, &head, "my own"]) else {
        panic!("an agent may not approve");
    };
    assert!(refused.starts_with("NOPERM "), "{refused}");

    let view = committer.reader().view();
    assert_eq!(
        view.get(MAIN_BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"30"[..]))
    );
}

#[test]
fn an_approver_merges_the_change_and_is_recorded() {
    let dir = TempDir::new("approve");
    let root = KeyPair::new();
    let (committer, stream) = start(&dir, &root);
    let mut client = Client::new(stream);
    let (branch, head) = the_change(&mut client);
    assert_eq!(
        client.ask(&["AUTH", &approver(&root)]),
        Resp::Simple("OK".to_owned())
    );

    let Resp::Bulk(Some(merge)) = client.ask(&["UMBRA.APPROVE", &branch, &head, "checked 8812"])
    else {
        panic!("an approval answers the merge's id");
    };

    assert_eq!(client.ask(&["UMBRA.PENDING"]), Resp::Array(Vec::new()));
    let Resp::Bulk(Some(log)) = client.ask(&["UMBRA.LOG", MAIN_BRANCH]) else {
        panic!("LOG answers JSON");
    };
    assert!(log.contains(r#""approved_by":["person:lead"]"#), "{log}");
    let view = committer.reader().view();
    assert_eq!(
        view.get(MAIN_BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"530"[..]))
    );
    let store = committer.stop();
    let main = store.log(MAIN_BRANCH)[0];
    assert!(
        merge.starts_with(&umbrachyx::store::short(&main.id)),
        "{merge}"
    );
    assert_eq!(main.provenance.who, "person:lead");
    assert_eq!(main.provenance.why, "checked 8812");
    assert_eq!(main.provenance.approved_by.as_deref(), Some("person:lead"));
}

#[test]
fn an_approver_outside_a_changed_key_is_refused() {
    let dir = TempDir::new("narrow");
    let root = KeyPair::new();
    let (committer, stream) = start(&dir, &root);
    let mut client = Client::new(stream);
    let (branch, head) = the_change(&mut client);
    let narrow = token(
        &root,
        &[Operation::Read, Operation::Approve],
        "person:lead",
        "guild/",
    );
    assert_eq!(
        client.ask(&["AUTH", &narrow]),
        Resp::Simple("OK".to_owned())
    );

    let Resp::Error(refused) = client.ask(&["UMBRA.APPROVE", &branch, &head, "not mine"]) else {
        panic!("refused");
    };

    assert!(refused.starts_with("NOPERM "), "{refused}");
    let view = committer.reader().view();
    assert_eq!(
        view.get(MAIN_BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"30"[..]))
    );
}

#[test]
fn a_head_that_moved_is_not_approved() {
    let dir = TempDir::new("moved");
    let root = KeyPair::new();
    let (committer, stream) = start(&dir, &root);
    let mut client = Client::new(stream);
    let (branch, head) = the_change(&mut client);
    committer
        .writer()
        .commit(
            AGENT_BRANCH,
            vec![set("player/7/gems", "99999")],
            by("agent:support", "one more"),
        )
        .expect("commits");
    assert_eq!(
        client.ask(&["AUTH", &approver(&root)]),
        Resp::Simple("OK".to_owned())
    );

    let Resp::Error(refused) = client.ask(&["UMBRA.APPROVE", &branch, &head, "as seen"]) else {
        panic!("refused");
    };

    assert!(refused.starts_with("NOTWAITING moved "), "{refused}");
    let view = committer.reader().view();
    assert_eq!(
        view.get(MAIN_BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"30"[..]))
    );
}

#[test]
fn a_conflict_names_its_keys_and_merges_nothing() {
    let dir = TempDir::new("conflict");
    let root = KeyPair::new();
    let (committer, stream) = start(&dir, &root);
    committer
        .writer()
        .commit(
            MAIN_BRANCH,
            vec![set("player/7/gems", "31")],
            by("game:shop", "potion"),
        )
        .expect("commits");
    let mut client = Client::new(stream);
    let (branch, head) = the_change(&mut client);
    assert_eq!(
        client.ask(&["AUTH", &approver(&root)]),
        Resp::Simple("OK".to_owned())
    );

    let Resp::Error(refused) = client.ask(&["UMBRA.APPROVE", &branch, &head, "go"]) else {
        panic!("refused");
    };

    assert_eq!(refused, "CONFLICT player/7/gems");
    let view = committer.reader().view();
    assert_eq!(
        view.get(MAIN_BRANCH, b"player/7/gems"),
        Some(Bytes::from(&b"31"[..]))
    );
}

#[test]
fn an_approver_rejects_and_the_change_leaves_the_list() {
    let dir = TempDir::new("reject");
    let root = KeyPair::new();
    let (committer, stream) = start(&dir, &root);
    let mut client = Client::new(stream);
    let (branch, head) = the_change(&mut client);
    assert_eq!(
        client.ask(&["AUTH", &approver(&root)]),
        Resp::Simple("OK".to_owned())
    );

    let rejected = client.ask(&["UMBRA.REJECT", &branch, &head, "wrong player"]);

    assert_eq!(rejected, Resp::Simple("OK".to_owned()));
    assert_eq!(client.ask(&["UMBRA.PENDING"]), Resp::Array(Vec::new()));
    let store = committer.stop();
    let rejection = store.rejection(AGENT_BRANCH).expect("recorded");
    assert_eq!(rejection.provenance.who, "person:lead");
    assert_eq!(rejection.provenance.why, "wrong player");
}
