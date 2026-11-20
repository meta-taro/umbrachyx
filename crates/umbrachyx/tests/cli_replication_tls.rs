//! `serve` with certificates: two servers started from the command line
//! speak TLS to each other, and the replica reads a player first written
//! after its backup. The certificates are made by the test into its own
//! temporary directory, and removed with it. The data is synthetic.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};

const BRANCH: &str = "app";
const LIMIT: Duration = Duration::from_secs(30);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-cli-tls-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().expect("utf-8 path").to_owned()
    }

    fn umbrachyx(&self, db: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_umbrachyx"));
        command.args(args).env("UMBRACHYX_DB", self.0.join(db));
        command
    }

    fn run(&self, db: &str, args: &[&str]) -> Output {
        self.umbrachyx(db, args).output().expect("runs")
    }

    /// An authority, and a certificate it signed for `127.0.0.1` with
    /// its key, as the three files `serve` is given. Returns the options
    /// that name them.
    fn certificates(&self) -> [String; 6] {
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
        std::fs::write(self.0.join("ca.pem"), authority_pem).expect("writes");
        std::fs::write(self.0.join("cert.pem"), certificate).expect("writes");
        std::fs::write(self.0.join("key.pem"), key.serialize_pem()).expect("writes");
        [
            "--replication-cert".to_owned(),
            self.path("cert.pem"),
            "--replication-key".to_owned(),
            self.path("key.pem"),
            "--replication-ca".to_owned(),
            self.path("ca.pem"),
        ]
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server, killed when the test ends.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One test of this file at a time: each picks free ports and then
/// starts servers on them, and two doing that at once can pick the same.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn alone() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    listener.local_addr().expect("has an address").port()
}

/// Starts `serve` on `db` and a free port, and returns it with what it
/// said before it served.
fn serve(dir: &TempDir, db: &str, options: &[&str]) -> (Server, String, u16) {
    let port = free_port();
    let port_text = port.to_string();
    let mut args = vec!["serve", BRANCH, "--port", &port_text];
    args.extend_from_slice(options);
    let mut child = dir
        .umbrachyx(db, &args)
        .stderr(Stdio::piped())
        .spawn()
        .expect("starts");
    let mut stderr = BufReader::new(child.stderr.take().expect("piped"));
    let server = Server(child);
    let mut said = String::new();
    loop {
        let mut line = String::new();
        let read = stderr.read_line(&mut line).expect("reads");
        assert!(read > 0, "the server ended, having said: {said}");
        said.push_str(&line);
        if line.starts_with("serving ") {
            break;
        }
    }
    // Whatever it says later is read and dropped, so it never blocks.
    thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = stderr.read_to_end(&mut rest);
    });
    (server, said, port)
}

fn connect(port: u16) -> TcpStream {
    let start = Instant::now();
    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            return stream;
        }
        assert!(start.elapsed() < LIMIT, "nothing listens on {port}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Sends one command and returns the first line of its reply, or of its
/// value.
fn first_line(client: &mut TcpStream, command: &[&str]) -> String {
    let mut out = format!("*{}\r\n", command.len());
    for arg in command {
        out.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
    }
    client.write_all(out.as_bytes()).expect("sends");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).expect("reads");
    if line.starts_with('$') && !line.starts_with("$-1") {
        line.clear();
        reader.read_line(&mut line).expect("reads");
    }
    line.trim_end().to_owned()
}

/// Asks again until the reply to `command` holds `wanted`.
fn wait_for(client: &mut TcpStream, command: &[&str], wanted: &str) -> String {
    let start = Instant::now();
    loop {
        let reply = first_line(client, command);
        if reply.contains(wanted) {
            return reply;
        }
        assert!(
            start.elapsed() < LIMIT,
            "{command:?} was never answered with {wanted:?}, but {reply:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn two_servers_given_certificates_send_a_new_players_key_to_the_replica() {
    let _alone = alone();
    let dir = TempDir::new("pair");
    let certificates = dir.certificates();
    let certificates: Vec<&str> = certificates.iter().map(String::as_str).collect();
    let backup = dir.path("backup");
    let feed_port = free_port().to_string();
    let mut options = vec![
        "--feed-replicas-on",
        &feed_port,
        "--backup-to",
        &backup,
        "--backup-every",
        "0.05",
    ];
    options.extend_from_slice(&certificates);
    assert!(
        dir.run("primary", &["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let (_primary, said, port) = serve(&dir, "primary", &options);
    assert!(said.contains("replication is over TLS"), "{said}");
    let mut primary = connect(port);
    assert_eq!(
        first_line(&mut primary, &["SET", "player/1/gems", "55"]),
        "+OK"
    );
    // A replica starts as a restored backup: one that holds the player.
    let start = Instant::now();
    loop {
        let _ = std::fs::remove_file(dir.0.join("replica"));
        let _ = std::fs::remove_file(dir.0.join("replica.keys"));
        let restored = dir.run("replica", &["restore", &backup]);
        let read = dir.run("replica", &["get", BRANCH, "player/1/gems"]);
        if restored.status.success() && String::from_utf8_lossy(&read.stdout).trim() == "55" {
            break;
        }
        assert!(start.elapsed() < LIMIT, "no backup");
        thread::sleep(Duration::from_millis(50));
    }
    let follow = format!("127.0.0.1:{feed_port}");
    let mut options = vec!["--follow", &follow];
    options.extend_from_slice(&certificates);
    let (_replica, _, replica_port) = serve(&dir, "replica", &options);
    let mut replica = connect(replica_port);

    // A player the primary first hears of after the backup.
    assert_eq!(
        first_line(&mut primary, &["SET", "player/2/gems", "7"]),
        "+OK"
    );

    wait_for(&mut replica, &["GET", "player/2/gems"], "7");
    let said = first_line(&mut replica, &["UMBRA.REPLICATION"]);
    assert!(said.contains("\"encrypted\":true"), "{said}");
    assert!(said.contains("\"stopped\":null"), "{said}");
}

#[test]
fn serve_without_certificates_says_that_nothing_between_servers_is_encrypted() {
    let _alone = alone();
    let dir = TempDir::new("clear");
    assert!(
        dir.run("primary", &["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    assert!(
        dir.run("alone", &["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let feed_port = free_port().to_string();

    let (_feeding, said, _) = serve(&dir, "primary", &["--feed-replicas-on", &feed_port]);
    let (_alone, said_alone, _) = serve(&dir, "alone", &[]);

    assert!(
        said.contains("nothing between this server and its replicas or primary is encrypted"),
        "{said}"
    );
    assert!(!said_alone.contains("encrypted"), "{said_alone}");
}

#[test]
fn serve_refuses_certificates_it_cannot_use() {
    let _alone = alone();
    let dir = TempDir::new("refused");
    let certificates = dir.certificates();
    let [cert, cert_file, key, key_file, ca, ca_file] = certificates.each_ref().map(String::as_str);
    let feed_port = free_port().to_string();
    let feeds = ["serve", BRANCH, "--feed-replicas-on", &feed_port];
    let run = |more: &[&str]| {
        let output = dir.run("primary", &[&feeds[..], more].concat());
        assert_eq!(output.status.code(), Some(2), "{more:?}");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    assert!(
        dir.run("primary", &["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let missing = dir.path("no-such.pem");

    let without_key = run(&[cert, cert_file, ca, ca_file]);
    let without_authority = run(&[cert, cert_file, key, key_file]);
    let no_file = run(&[cert, &missing, key, key_file, ca, ca_file]);
    let not_a_key = run(&[cert, cert_file, key, cert_file, ca, ca_file]);
    let another_key = run(&[cert, ca_file, key, key_file, ca, ca_file]);
    let nobody_to_talk_to = dir.run(
        "primary",
        &["serve", BRANCH, cert, cert_file, key, key_file, ca, ca_file],
    );

    assert!(
        without_key.contains("needs --replication-key too"),
        "{without_key}"
    );
    assert!(
        without_authority.contains("needs --replication-ca too"),
        "{without_authority}"
    );
    assert!(
        no_file.contains("the certificate, ") && no_file.contains("no-such.pem"),
        "{no_file}"
    );
    assert!(
        not_a_key.contains("the private key cannot be read"),
        "{not_a_key}"
    );
    assert!(
        another_key.contains("the certificate and its key cannot be used"),
        "{another_key}"
    );
    let said = String::from_utf8_lossy(&nobody_to_talk_to.stderr);
    assert!(
        said.contains("needs --feed-replicas-on or --follow"),
        "{said}"
    );
    // What it could not use, it did not print.
    for said in [&not_a_key, &another_key] {
        assert!(!said.contains("BEGIN"), "{said}");
    }
}
