//! `SET`s to a plain-async table over the server: the thread that reads
//! them holds them itself and answers at once, sealing their values
//! without the store. What must hold whichever thread held a put: the
//! reply order, a read of one's own write, and that the log replays to
//! what readers were shown.
//!
//! The data is synthetic: a cache, sessions and players.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
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
            "umbrachyx-server-plain-early-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn open(&self, is_sealed: bool) -> FileStore {
        let storage = FileStorage::open(&self.0.join("log")).expect("opens file");
        if !is_sealed {
            return Store::open(storage, SystemClock).expect("opens store");
        }
        let keys: Box<dyn KeyStorage + Send> =
            Box::new(FileKeyStorage::new(&self.0.join("log.keys")));
        let keys = Keys::open(keys).expect("opens the key file");
        Store::open_sealed(storage, keys, SystemClock).expect("opens store")
    }

    /// A store with `cache` declared plain-async, `session` plain, and
    /// `BRANCH` made from main.
    fn store(&self, is_sealed: bool) -> FileStore {
        let mut store = self.open(is_sealed);
        for (table, engine) in [("cache", Engine::PlainAsync), ("session", Engine::Plain)] {
            let provenance = Provenance {
                who: "test".to_owned(),
                why: "it needs no past".to_owned(),
                approved_by: None,
            };
            store
                .declare_table(table.as_bytes(), engine, provenance)
                .expect("declares");
        }
        store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
        store
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serves `BRANCH` of `store` on a free port of this machine, with two
/// serving threads, writing puts answered early within `sync_every`.
fn serve(
    store: FileStore,
    sync_every: Duration,
) -> (Committer<FileStorage, SystemClock>, SocketAddr) {
    let committer = Committer::start(store);
    let server = Server::new(BRANCH, committer.writer(), committer.reader())
        .expect("serves a branch")
        .threads(2)
        .sync_every(sync_every);
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    thread::spawn(move || server.serve(listener));
    (committer, address)
}

const A_LONG_TIME: Duration = Duration::from_secs(3600);

fn connect(address: SocketAddr) -> TcpStream {
    let client = TcpStream::connect(address).expect("connects");
    client
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("sets a timeout");
    client
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

/// The value a `GET` is answered with, after its length line.
fn read_value(client: &mut TcpStream) -> Option<String> {
    let line = read_line(client);
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

fn get(client: &mut TcpStream, key: &str) -> Option<String> {
    client.write_all(&command(&["GET", key])).expect("sends");
    read_value(client)
}

fn text(value: Option<umbrachyx::tree::Bytes>) -> Option<String> {
    value.map(|bytes| String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

/// The path is taken: nearly every put from a client alone is held by
/// the thread that read it. Now and then one finds the thread that
/// writes at work and goes to it, so not every one is asked for.
#[test]
fn sets_to_a_plain_async_table_are_held_by_the_thread_that_read_them() {
    const PUTS: usize = 200;
    let dir = TempDir::new("held");
    let (committer, address) = serve(dir.store(false), A_LONG_TIME);
    let mut client = connect(address);

    for n in 0..PUTS {
        let key = format!("cache/{}/top", n % 10);
        assert_eq!(ask_line(&mut client, &["SET", &key, &n.to_string()]), "+OK");
    }

    let held = committer.writer().puts_held_early();
    assert!(held >= 150, "{held} of {PUTS} puts were held where read");
    assert_eq!(get(&mut client, "cache/9/top"), Some("199".to_owned()));
    drop(committer.stop());
    let store = dir.open(false);
    for n in 0..10 {
        let value = text(store.get(BRANCH, format!("cache/{n}/top").as_bytes()));
        assert_eq!(value, Some((190 + n).to_string()));
    }
}

/// Only the store makes a key. The first put for a subject goes to it;
/// once it has sealed for the subject, the thread that reads a put seals
/// it, to the bytes the store would have, and it opens as any other.
#[test]
fn a_subject_with_a_key_is_sealed_for_by_the_thread_that_reads_the_put() {
    let dir = TempDir::new("sealed");
    let (committer, address) = serve(dir.store(true), A_LONG_TIME);
    let mut client = connect(address);
    let writer = committer.writer();

    assert_eq!(
        ask_line(&mut client, &["SET", "cache/7/top", "first"]),
        "+OK"
    );
    assert_eq!(writer.puts_held_early(), 0, "a new subject is the store's");
    for n in 0..50 {
        let key = format!("cache/7/item{n}");
        assert_eq!(
            ask_line(&mut client, &["SET", &key, &format!("v{n}")]),
            "+OK"
        );
    }
    assert_eq!(
        ask_line(&mut client, &["SET", "cache/7/top", "last"]),
        "+OK"
    );

    let held = writer.puts_held_early();
    assert!(held >= 40, "{held} of 51 puts were held where read");
    assert_eq!(get(&mut client, "cache/7/top"), Some("last".to_owned()));
    assert_eq!(get(&mut client, "cache/7/item49"), Some("v49".to_owned()));
    drop(writer);
    drop(committer.stop());
    let store = dir.open(true);
    assert_eq!(
        text(store.get(BRANCH, b"cache/7/top")),
        Some("last".to_owned())
    );
    for n in 0..50 {
        let value = text(store.get(BRANCH, format!("cache/7/item{n}").as_bytes()));
        assert_eq!(value, Some(format!("v{n}")));
    }
}

/// Replies come in the order asked, whichever thread held each write,
/// and a read in the same pipeline sees the writes before it.
#[test]
fn a_pipeline_across_every_kind_of_table_is_answered_in_order() {
    let dir = TempDir::new("order");
    let (_committer, address) = serve(dir.store(true), A_LONG_TIME);
    let mut client = connect(address);
    // The subject has a key from here on.
    assert_eq!(ask_line(&mut client, &["SET", "cache/1/top", "0"]), "+OK");
    let mut requests = Vec::new();
    for round in 0..20 {
        let value = round.to_string();
        requests.extend(command(&["SET", "cache/1/top", &value]));
        requests.extend(command(&["GET", "cache/1/top"]));
        requests.extend(command(&["SET", "cache/1/top", "between"]));
        requests.extend(command(&["SET", "session/1/token", &value]));
        requests.extend(command(&["SET", "cache/1/top", &value]));
        requests.extend(command(&["SET", "player/1/gold", &value]));
        requests.extend(command(&["SET", "cache/1/other", &value]));
        requests.extend(command(&["GET", "cache/1/top"]));
        requests.extend(command(&["GET", "session/1/token"]));
        requests.extend(command(&["GET", "player/1/gold"]));
    }

    client.write_all(&requests).expect("sends");

    for round in 0..20 {
        let value = Some(round.to_string());
        assert_eq!(read_line(&mut client), "+OK");
        assert_eq!(read_value(&mut client), value, "round {round}");
        for _ in 0..5 {
            assert_eq!(read_line(&mut client), "+OK");
        }
        for _ in 0..3 {
            assert_eq!(read_value(&mut client), value, "round {round}");
        }
    }
}

/// Several clients write the same keys at once, with values large
/// enough that the store soon holds as much ahead of the disk as it
/// may: some puts are held by the thread that read them and some go to
/// the thread that writes. Whichever, the last value a reader is shown
/// for a key is the one the log replays to.
#[test]
fn clients_writing_the_same_keys_at_once_leave_what_the_log_replays() {
    const CLIENTS: usize = 4;
    const ROUNDS: usize = 60;
    const KEYS: usize = 6;
    const VALUE_LEN: usize = 256 * 1024;
    for is_sealed in [false, true] {
        let dir = TempDir::new(if is_sealed { "same-sealed" } else { "same" });
        let (committer, address) = serve(dir.store(is_sealed), Duration::from_millis(50));
        let mut client = connect(address);
        for key in 0..KEYS {
            // Every subject has a key before the clients start.
            let key = format!("cache/{key}/blob");
            assert_eq!(ask_line(&mut client, &["SET", &key, "start"]), "+OK");
        }

        let writers: Vec<_> = (0..CLIENTS)
            .map(|id| {
                thread::spawn(move || {
                    let mut client = connect(address);
                    for round in 0..ROUNDS {
                        let key = format!("cache/{}/blob", (id + round) % KEYS);
                        let mark = format!("{id}-{round}-");
                        let value = mark.repeat(VALUE_LEN / mark.len());
                        assert_eq!(ask_line(&mut client, &["SET", &key, &value]), "+OK");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a client ends");
        }

        let shown: BTreeMap<String, Option<String>> = (0..KEYS)
            .map(|key| format!("cache/{key}/blob"))
            .map(|key| (key.clone(), get(&mut client, &key)))
            .collect();
        let held = committer.writer().puts_held_early();
        let all = (CLIENTS * ROUNDS) as u64;
        assert!(held > 0, "no put was held where it was read");
        assert!(
            held < all + KEYS as u64,
            "no put went to the thread that writes"
        );
        drop(committer.stop());
        let store = dir.open(is_sealed);
        for (key, value) in shown {
            assert!(value.as_ref().is_some_and(|value| value.len() > 1000));
            let replayed = text(store.get(BRANCH, key.as_bytes()));
            assert!(replayed == value, "{key} replays to what was shown");
        }
    }
}
