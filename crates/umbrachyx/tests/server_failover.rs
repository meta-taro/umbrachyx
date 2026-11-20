//! The primary is killed while clients write to it, and every write it
//! acknowledged is on its replica.
//!
//! The primary is started to answer a write only once a replica has said
//! it holds it (`serve --wait-for-replica`), so an acknowledged write is
//! on two disks. Each round starts a primary and a replica made from its
//! backup, lets clients write, kills the primary with SIGKILL at a
//! random moment, and reads every acknowledged write from the replica.
//!
//! That this can fail was checked by hand: with `UMBRACHYX_FAILOVER_NO_WAIT`
//! set, the primary answers without waiting for the replica, and the
//! test finds acknowledged writes the replica does not hold.
//!
//! The same is run with the two servers showing each other
//! certificates, and every write to a subject of its own: each needs a
//! key the replica was never given, which it asks its primary for before
//! it can say it holds the write. The certificates are made by the test
//! into its temporary directory and removed with it.
//!
//! `cargo test --release --test server_failover -- --ignored` runs 100
//! rounds of each. The data is synthetic.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

const BRANCH: &str = "bench";
/// Every key is this subject's, whose key the replica is given with the
/// backup it starts from: keys are not sent to replicas yet.
const SUBJECT: &str = "failover/test";
const ROUNDS_ENV: &str = "UMBRACHYX_FAILOVER_ROUNDS";
const NO_WAIT_ENV: &str = "UMBRACHYX_FAILOVER_NO_WAIT";
const QUICK_ROUNDS: u32 = 3;
const FULL_ROUNDS: u32 = 100;
const CLIENTS: usize = 8;
const PIPELINE: usize = 16;
const MIN_KILL_DELAY_MILLIS: u64 = 20;
const MAX_KILL_DELAY_MILLIS: u64 = 300;
const SEED: u64 = 0x5EED_0003;
const START_TIMEOUT: Duration = Duration::from_secs(120);
const START_WAIT: Duration = Duration::from_millis(25);
const FIRST_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the primary waits for a replica: longer than a round.
const WAIT_FOR_REPLICA: &str = "60";

/// What lies between the two servers of a round.
#[derive(Clone, Copy, PartialEq)]
enum Between {
    /// Nothing encrypted, no keys sent: every write to one subject,
    /// whose key the replica has from its backup.
    Clear,
    /// TLS with certificates, and every write to a subject of its own.
    Certificates,
}

impl Between {
    fn key(self, client: usize, seq: usize) -> String {
        match self {
            Self::Clear => format!("{SUBJECT}/{client}-{seq}"),
            Self::Certificates => format!("failover-{client}/{seq}/gems"),
        }
    }
}

/// Makes an authority and a certificate it signed for `127.0.0.1`, as
/// files in `dir`, and returns the options that name them.
fn certificates(dir: &TempDir) -> Vec<String> {
    let authority_key = KeyPair::generate().expect("makes a key");
    let mut authority = CertificateParams::new(Vec::new()).expect("no names");
    authority.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let authority_pem = authority.self_signed(&authority_key).expect("signs").pem();
    let issuer = Issuer::new(authority, authority_key);
    let key = KeyPair::generate().expect("makes a key");
    let certificate = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("a name")
        .signed_by(&key, &issuer)
        .expect("signs")
        .pem();
    let files = [
        ("--replication-ca", "ca.pem", authority_pem),
        ("--replication-cert", "cert.pem", certificate),
        ("--replication-key", "key.pem", key.serialize_pem()),
    ];
    files
        .into_iter()
        .flat_map(|(option, name, pem)| {
            let path = dir.path(name);
            std::fs::write(&path, pem).expect("writes");
            [
                option.to_owned(),
                path.to_str().expect("utf-8 path").to_owned(),
            ]
        })
        .collect()
}

fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn umbrachyx(log: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_umbrachyx"));
    command
        .args(args)
        .env("UMBRACHYX_DB", log)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    listener.local_addr().expect("has an address").port()
}

fn set(key: &str, value: &str) -> Vec<u8> {
    format!(
        "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{value}\r\n",
        key.len(),
        value.len()
    )
    .into_bytes()
}

/// A fresh directory per round, removed when the round ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-failover-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: a leftover directory in the temp dir harms nothing.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the server when dropped, so a failed round leaves no process.
struct Server(Child);

impl Server {
    fn kill(mut self) {
        self.0.kill().expect("kills the server");
        self.0.wait().expect("reaps the server");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(log: &Path, args: &[&str]) {
    let status = umbrachyx(log, args).status().expect("runs");
    assert!(status.success(), "{args:?} fails");
}

/// Starts `serve` with `options` and waits until it listens on `port`.
fn serve(log: &Path, errors: &Path, port: u16, options: &[&str]) -> Server {
    let port_text = port.to_string();
    let mut args = vec!["serve", BRANCH, "--port", &port_text];
    args.extend_from_slice(options);
    let mut server = Server(
        umbrachyx(log, &args)
            .stderr(File::create(errors).expect("creates the error file"))
            .spawn()
            .expect("starts the server"),
    );
    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return server;
        }
        if let Some(status) = server.0.try_wait().expect("checks the server") {
            panic!(
                "the server exited with {status}: {}",
                std::fs::read_to_string(errors).unwrap_or_default()
            );
        }
        thread::sleep(START_WAIT);
    }
    panic!("the server did not listen on {port}");
}

/// A primary that waits for its replica, and the replica, made from a
/// backup of it. Returns both and the ports clients use.
fn pair(dir: &TempDir, between: Between) -> (Server, u16, Server, u16) {
    let (primary, replica, backup) = (dir.path("primary"), dir.path("replica"), dir.path("backup"));
    let backup = backup.to_str().expect("utf-8 path");
    run(&primary, &["branch", BRANCH, "--from", "main"]);
    let seed = format!("{SUBJECT}/seed=0");
    run(
        &primary,
        &[
            "commit",
            BRANCH,
            &seed,
            "--who",
            "seed",
            "--why",
            "test data",
        ],
    );
    run(&primary, &["backup", backup]);
    run(&replica, &["restore", backup]);
    let (port, feed, replica_port) = (free_port(), free_port(), free_port());
    let feed_text = feed.to_string();
    let mut options = vec!["--feed-replicas-on", feed_text.as_str()];
    if std::env::var_os(NO_WAIT_ENV).is_none() {
        options.extend(["--wait-for-replica", WAIT_FOR_REPLICA]);
    }
    let certificates = match between {
        Between::Clear => Vec::new(),
        Between::Certificates => certificates(dir),
    };
    options.extend(certificates.iter().map(String::as_str));
    let primary = serve(&primary, &dir.path("primary.err"), port, &options);
    let follow = format!("127.0.0.1:{feed}");
    let mut options = vec!["--follow", follow.as_str()];
    options.extend(certificates.iter().map(String::as_str));
    let replica = serve(&replica, &dir.path("replica.err"), replica_port, &options);
    (primary, port, replica, replica_port)
}

/// Writes until the primary goes away; returns the writes it
/// acknowledged, and says so once it has one.
fn client(
    port: u16,
    client: usize,
    between: Between,
    has_written: &AtomicBool,
) -> Vec<(String, String)> {
    let mut acknowledged = Vec::new();
    let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return acknowledged;
    };
    let mut writer = stream.try_clone().expect("clones the stream");
    let mut replies = BufReader::new(stream);
    let depth = if client.is_multiple_of(2) {
        1
    } else {
        PIPELINE
    };
    let mut line = String::new();
    for batch in 0.. {
        let writes: Vec<_> = (0..depth)
            .map(|n| {
                let seq = batch * depth + n;
                (between.key(client, seq), format!("{client}-{seq}"))
            })
            .collect();
        let request: Vec<u8> = writes.iter().flat_map(|(k, v)| set(k, v)).collect();
        if writer.write_all(&request).is_err() {
            return acknowledged;
        }
        for write in writes {
            line.clear();
            match replies.read_line(&mut line) {
                Ok(_) if line == "+OK\r\n" => {
                    acknowledged.push(write);
                    has_written.store(true, Ordering::Relaxed);
                }
                // Killed mid-reply, or never answered: not acknowledged.
                _ => return acknowledged,
            }
        }
    }
    acknowledged
}

/// What the replica answers a `GET` of `key` with.
fn get(replica: &mut BufReader<TcpStream>, key: &str) -> Option<String> {
    let request = format!("*2\r\n$3\r\nGET\r\n${}\r\n{key}\r\n", key.len());
    replica
        .get_mut()
        .write_all(request.as_bytes())
        .expect("asks the replica");
    let mut line = String::new();
    replica.read_line(&mut line).expect("reads the replica");
    let len: usize = line.trim_end().strip_prefix('$')?.parse().ok()?;
    let mut value = vec![0; len + 2];
    replica.read_exact(&mut value).expect("reads the value");
    value.truncate(len);
    String::from_utf8(value).ok()
}

fn rounds() -> u32 {
    std::env::var(ROUNDS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(FULL_ROUNDS)
}

fn kill_rounds(name: &str, rounds: u32, between: Between) {
    let mut random = SEED;
    let mut total = 0;
    for round in 0..rounds {
        let dir = TempDir::new(&format!("{name}-{round}"));
        let span = MAX_KILL_DELAY_MILLIS - MIN_KILL_DELAY_MILLIS;
        let delay = MIN_KILL_DELAY_MILLIS + next_random(&mut random) % span;
        let (primary, port, _replica, replica_port) = pair(&dir, between);
        let has_written = Arc::new(AtomicBool::new(false));
        let clients: Vec<_> = (0..CLIENTS)
            .map(|n| {
                let has_written = Arc::clone(&has_written);
                thread::spawn(move || client(port, n, between, &has_written))
            })
            .collect();
        let started = Instant::now();
        while !has_written.load(Ordering::Relaxed) && started.elapsed() < FIRST_WRITE_TIMEOUT {
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(delay));
        primary.kill();
        let acknowledged: Vec<_> = clients
            .into_iter()
            .flat_map(|handle| handle.join().expect("client finishes"))
            .collect();

        // At once: an acknowledged write was on the replica before its
        // writer heard of it.
        let mut replica = BufReader::new(
            TcpStream::connect(("127.0.0.1", replica_port)).expect("the replica still answers"),
        );
        for (key, value) in &acknowledged {
            assert_eq!(
                get(&mut replica, key).as_deref(),
                Some(value.as_str()),
                "round {round} (seed {SEED:#x}): acknowledged {key} is not on the replica; it \
                 said: {}",
                std::fs::read_to_string(dir.path("replica.err")).unwrap_or_default()
            );
        }
        assert!(
            !acknowledged.is_empty(),
            "round {round}: no write was acknowledged; the primary said: {}",
            std::fs::read_to_string(dir.path("primary.err")).unwrap_or_default()
        );
        total += acknowledged.len();
    }
    let how = match between {
        Between::Clear => "",
        Between::Certificates => " over TLS, each SET to a subject of its own",
    };
    println!(
        "{rounds} rounds{how}, {total} acknowledged SETs, all on the replica when the primary \
         was killed"
    );
}

#[test]
fn quick_kill_the_primary_during_writes() {
    kill_rounds("quick", QUICK_ROUNDS, Between::Clear);
}

#[test]
#[ignore = "100 rounds; run with --ignored"]
fn kill_the_primary_during_writes() {
    kill_rounds("full", rounds(), Between::Clear);
}

#[test]
fn quick_kill_the_primary_during_writes_that_each_need_a_new_key() {
    kill_rounds("quick-tls", QUICK_ROUNDS, Between::Certificates);
}

#[test]
#[ignore = "100 rounds; run with --ignored"]
fn kill_the_primary_during_writes_that_each_need_a_new_key() {
    kill_rounds("full-tls", rounds(), Between::Certificates);
}
