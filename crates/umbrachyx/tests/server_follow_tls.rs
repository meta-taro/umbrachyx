//! Two servers that show each other certificates: the replica is sent
//! the keys of subjects first written after it was made, and reads them.
//! A server that shows no certificate the authority signed is told
//! nothing, and without certificates no key is sent at all.
//!
//! The certificates are made by each test, in memory, and gone with it.
//! The data is synthetic: players and their gems.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::server::{Identity, Server};
use umbrachyx::store::backup::{DirSource, DirTarget, restore};
use umbrachyx::store::{Committer, MAIN_BRANCH, Provenance, Store, SystemClock};

const BRANCH: &str = "bench";
/// How long a replica gets to hold what its primary was sent. It takes
/// milliseconds; this only has to be longer than a busy machine makes it.
const LIMIT: Duration = Duration::from_secs(30);
const AGAIN: Duration = Duration::from_millis(5);
/// Where every server of these tests listens, and so the name their
/// certificates carry.
const HOST: &str = "127.0.0.1";

type FileStore = Store<FileStorage, SystemClock>;

/// An authority made for one test.
struct Authority {
    certificate: String,
    issuer: Issuer<'static, KeyPair>,
}

impl Authority {
    fn new() -> Self {
        let key = KeyPair::generate().expect("makes a key");
        let mut params = CertificateParams::new(Vec::new()).expect("no names");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let certificate = params.self_signed(&key).expect("signs itself").pem();
        Self {
            certificate,
            issuer: Issuer::new(params, key),
        }
    }

    /// What a server at [`HOST`] shows, signed by this authority, and
    /// asks of the other end that it be signed by `trusted`.
    fn identity(&self, trusted: &Authority) -> Identity {
        let key = KeyPair::generate().expect("makes a key");
        let params = CertificateParams::new(vec![HOST.to_owned()]).expect("a name");
        let certificate = params.signed_by(&key, &self.issuer).expect("signs").pem();
        Identity::from_pem(
            certificate.as_bytes(),
            key.serialize_pem().as_bytes(),
            trusted.certificate.as_bytes(),
        )
    }
}

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-server-follow-tls-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    /// A store whose values are encrypted, with the branch served and
    /// one player.
    fn primary(&self) -> FileStore {
        let mut store = open(&self.0.join("primary"));
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
        store
    }

    /// A backup of `from` restored as another store, with the keys it
    /// had then: how a replica starts.
    fn copy_of(&self, from: &mut FileStore) -> FileStore {
        let backup = self.0.join("backup");
        let mut target = DirTarget::open(&backup).expect("opens the backup");
        from.backup(&mut target).expect("backs up");
        // The place is locked while it is written to, and on Windows
        // nothing reads it until that ends.
        drop(target);
        let path = self.0.join("replica");
        let mut log = FileStorage::open(&path).expect("opens file");
        let mut keys = FileKeyStorage::new(&keys_of(&path));
        restore(&mut DirSource::new(&backup), &mut log, Some(&mut keys)).expect("restores");
        drop(log);
        open(&path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn keys_of(log: &Path) -> PathBuf {
    log.with_extension("keys")
}

fn open(path: &Path) -> FileStore {
    let storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(&keys_of(path)));
    let keys = Keys::open(storage).expect("opens the key file");
    let log = FileStorage::open(path).expect("opens file");
    Store::open_sealed(log, keys, SystemClock).expect("opens store")
}

fn free_address() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind((HOST, 0)).expect("binds");
    let address = listener.local_addr().expect("has an address");
    (listener, address)
}

/// Serves `store` as a primary that answers replicas, showing
/// `identity` if it has one. Returns where it answers clients and where
/// replicas.
fn serve_primary(store: FileStore, identity: Option<&Identity>) -> (SocketAddr, SocketAddr) {
    let committer = Committer::start(store);
    let (listener, address) = free_address();
    let (feed, feed_address) = free_address();
    let mut server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .feed_replicas_on(feed);
    if let Some(identity) = identity {
        server = server.replicate_as(identity).expect("takes the identity");
    }
    thread::spawn(move || {
        // Kept for as long as the server runs: stopping it would end
        // the store under the server.
        let _committer = committer;
        server.serve(listener)
    });
    (address, feed_address)
}

/// Serves `store` as a replica of the server at `primary`, and returns
/// where.
fn serve_replica(store: FileStore, primary: SocketAddr, identity: Option<&Identity>) -> SocketAddr {
    let committer = Committer::start_following(store);
    let mut server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .follow(primary.to_string());
    if let Some(identity) = identity {
        server = server.replicate_as(identity).expect("takes the identity");
    }
    let (listener, address) = free_address();
    thread::spawn(move || {
        let _committer = committer;
        server.serve(listener)
    });
    address
}

/// A primary and a replica of it, each with the identity given, and
/// where the primary answers replicas.
fn pair(
    dir: &TempDir,
    primary: Option<&Identity>,
    replica: Option<&Identity>,
) -> (Client, Client, SocketAddr) {
    let mut store = dir.primary();
    let copy = dir.copy_of(&mut store);
    let (address, feed) = serve_primary(store, primary);
    let replica = serve_replica(copy, feed, replica);
    (Client::connect(address), Client::connect(replica), feed)
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
fn over_tls_a_replica_reads_a_subject_first_written_after_it_was_made() {
    let dir = TempDir::new("keys");
    let authority = Authority::new();
    let (mut primary, mut replica, _) = pair(
        &dir,
        Some(&authority.identity(&authority)),
        Some(&authority.identity(&authority)),
    );

    assert_eq!(primary.text(&["SET", "player/2/gems", "7"]), "+OK");
    assert_eq!(primary.text(&["SET", "player/3/gems", "9"]), "+OK");

    replica.wait_for(&["GET", "player/3/gems"], "9");
    assert_eq!(replica.text(&["GET", "player/2/gems"]), "7");
    let said = replica.replication();
    assert_eq!(said["encrypted"], true, "{said}");
    assert!(said["stopped"].is_null(), "{said}");
    assert_eq!(primary.replication()["encrypted"], true);
}

#[test]
fn without_certificates_no_key_is_sent_and_the_replica_says_which_it_lacks() {
    let dir = TempDir::new("plain");
    let (mut primary, mut replica, feed) = pair(&dir, None, None);

    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");
    replica.wait_for(&["GET", "player/1/gems"], "150");
    assert_eq!(primary.text(&["SET", "player/2/gems", "7"]), "+OK");

    let said = replica.wait_for_replication(|said| !said["stopped"].is_null());
    let why = said["stopped"].as_str().expect("says why");
    assert!(why.contains("the key file lacks 1 key"), "{why}");
    assert_eq!(said["encrypted"], false, "{said}");
    assert_eq!(replica.text(&["GET", "player/2/gems"]), "(nil)");
    // And nobody who reaches the port is given one for the asking.
    let asked = Client::connect(feed).text(&["UMBRA.KEYS", "0000000000000000"]);
    assert!(
        asked.starts_with("-ERR this port answers replicas only"),
        "{asked}"
    );
}

#[test]
fn a_replica_signed_by_another_authority_is_told_nothing() {
    let dir = TempDir::new("stranger");
    let authority = Authority::new();
    let other = Authority::new();
    let (mut primary, mut replica, _) = pair(
        &dir,
        Some(&authority.identity(&authority)),
        // It trusts the primary's authority; its own certificate is
        // what the primary does not accept.
        Some(&other.identity(&authority)),
    );
    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");

    let said = replica.wait_for_replication(|said| !said["unreachable"].is_null());

    assert!(said["records"].is_null(), "{said}");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");
}

#[test]
fn a_replica_does_not_ask_a_primary_signed_by_another_authority() {
    let dir = TempDir::new("impostor");
    let authority = Authority::new();
    let other = Authority::new();
    let (_, mut replica, _) = pair(
        &dir,
        Some(&other.identity(&authority)),
        Some(&authority.identity(&authority)),
    );

    let said = replica.wait_for_replication(|said| !said["unreachable"].is_null());

    assert!(said["records"].is_null(), "{said}");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");
}

#[test]
fn a_replica_without_a_certificate_gets_nothing_from_a_primary_that_asks_for_one() {
    let dir = TempDir::new("bare");
    let authority = Authority::new();
    let (_, mut replica, feed) = pair(&dir, Some(&authority.identity(&authority)), None);

    let said = replica.wait_for_replication(|said| !said["unreachable"].is_null());

    assert!(said["records"].is_null(), "{said}");
    // Nor does anyone who sends a command in the clear: the connection
    // is closed without a word.
    let mut plain = TcpStream::connect(feed).expect("connects");
    plain
        .write_all(b"*2\r\n$10\r\nUMBRA.KEYS\r\n$16\r\n0000000000000000\r\n")
        .expect("sends");
    let mut answer = Vec::new();
    let _ = plain.read_to_end(&mut answer);
    assert!(
        !answer.starts_with(b"$") && !answer.starts_with(b"-"),
        "{answer:?}"
    );
}

#[test]
fn a_replica_with_a_certificate_does_not_ask_a_primary_that_shows_none() {
    let dir = TempDir::new("downgrade");
    let authority = Authority::new();
    let (mut primary, mut replica, _) = pair(&dir, None, Some(&authority.identity(&authority)));
    assert_eq!(primary.text(&["SET", "player/1/gems", "150"]), "+OK");

    let said = replica.wait_for_replication(|said| !said["unreachable"].is_null());

    assert!(said["records"].is_null(), "{said}");
    assert_eq!(replica.text(&["GET", "player/1/gems"]), "100");
}
