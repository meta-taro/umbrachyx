//! `umbrachyx replication`: asks a running server how its replication is
//! going and prints it, without opening any log. The servers are started
//! from the command line on ports the system chooses. The data is
//! synthetic.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BRANCH: &str = "app";
const LIMIT: Duration = Duration::from_secs(30);
const WHO: [&str; 4] = ["--who", "test", "--why", "a synthetic player"];

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-cli-status-{}-{name}",
            std::process::id()
        ));
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
        let output = self.umbrachyx(db, args).output().expect("runs");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// A store with one player on the branch the servers serve, and a
    /// second store restored from its backup: a primary and its replica
    /// before either is started.
    fn primary_and_replica(&self) {
        self.run("primary", &["branch", BRANCH, "--from", "main"]);
        let mut commit = vec!["commit", BRANCH, "player/1/gems=55"];
        commit.extend_from_slice(&WHO);
        self.run("primary", &commit);
        self.run("primary", &["backup", &self.path("backup")]);
        self.run("replica", &["restore", &self.path("backup")]);
    }

    /// What `umbrachyx replication` says of the server on `port`: its
    /// exit status, what it printed, and what it complained of.
    fn status(&self, port: u16) -> (i32, String, String) {
        said(
            &self
                .umbrachyx("no-such-log", &["replication", "--port", &port.to_string()])
                .output()
                .expect("runs"),
        )
    }

    /// Asks until the status holds `wanted`.
    fn status_with(&self, port: u16, wanted: &str) -> (i32, String, String) {
        let start = Instant::now();
        loop {
            let status = self.status(port);
            if status.1.contains(wanted) {
                return status;
            }
            assert!(start.elapsed() < LIMIT, "never said {wanted:?}: {status:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn said(output: &Output) -> (i32, String, String) {
    (
        output.status.code().expect("exited"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A server, killed when the test ends.
struct Server {
    child: Child,
    /// The port clients are served on.
    port: u16,
    /// Where replicas ask, if it feeds any.
    feed: Option<String>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `serve` on `db`, on ports the system chooses, and reads which
/// it was given from what the server says.
fn serve(dir: &TempDir, db: &str, options: &[&str]) -> Server {
    let mut args = vec!["serve", BRANCH, "--port", "0"];
    args.extend_from_slice(options);
    let mut child = dir
        .umbrachyx(db, &args)
        .stderr(Stdio::piped())
        .spawn()
        .expect("starts");
    let mut stderr = BufReader::new(child.stderr.take().expect("piped"));
    let mut server = Server {
        child,
        port: 0,
        feed: None,
    };
    let mut all = String::new();
    loop {
        let mut line = String::new();
        let read = stderr.read_line(&mut line).expect("reads");
        assert!(read > 0, "the server ended, having said: {all}");
        all.push_str(&line);
        let line = line.trim_end();
        if let Some(feed) = line.strip_prefix("handing the log to replicas that ask on ") {
            server.feed = Some(feed.to_owned());
        }
        if let Some(serving) = line.strip_prefix(&format!("serving {BRANCH} on ")) {
            let (_, port) = serving.rsplit_once(':').expect("an address");
            server.port = port.parse().expect("a port");
            break;
        }
    }
    // Whatever it says later is read and dropped, so it never blocks.
    thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = stderr.read_to_end(&mut rest);
    });
    server
}

fn primary(dir: &TempDir) -> Server {
    serve(dir, "primary", &["--feed-replicas-on", "0"])
}

fn replica(dir: &TempDir, primary: &Server, options: &[&str]) -> Server {
    let mut all = vec!["--follow", primary.feed.as_deref().expect("feeds")];
    all.extend_from_slice(options);
    serve(dir, "replica", &all)
}

/// Sends one command and returns the first line of its reply.
fn first_line(port: u16, command: &[&str]) -> String {
    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    let mut out = format!("*{}\r\n", command.len());
    for arg in command {
        out.push_str(&format!("${}\r\n{arg}\r\n", arg.len()));
    }
    client.write_all(out.as_bytes()).expect("sends");
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).expect("reads");
    line.trim_end().to_owned()
}

/// Listens on a port of its own and answers the first command sent with
/// `reply`, whatever was asked: a server that is not an Umbrachyx server.
fn answers(reply: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().expect("has an address").port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut asked = [0; 64];
            let _ = stream.read(&mut asked);
            let _ = stream.write_all(reply);
        }
    });
    port
}

#[test]
fn a_primary_says_what_it_is_while_its_own_log_is_named() {
    let dir = TempDir::new("primary");
    dir.primary_and_replica();
    let primary = primary(&dir);

    // The log the running server holds is named: the command must not
    // try to open it.
    let output = dir
        .umbrachyx(
            "primary",
            &["replication", "--port", &primary.port.to_string()],
        )
        .output()
        .expect("runs");
    let (code, out, err) = said(&output);

    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("role: primary\n"), "{out}");
    assert!(out.contains("holds: history\n"), "{out}");
    assert!(out.contains("feeds replicas: yes, in the clear\n"), "{out}");
    assert!(
        out.contains("answers a write once a replica holds it: no\n"),
        "{out}"
    );
}

#[test]
fn a_replica_that_has_everything_says_how_many_records_and_exits_zero() {
    let dir = TempDir::new("caught-up");
    dir.primary_and_replica();
    let primary = primary(&dir);
    let replica = replica(&dir, &primary, &[]);
    assert_eq!(
        first_line(primary.port, &["SET", "player/1/gems", "70"]),
        "+OK"
    );

    let (code, out, err) = dir.status_with(replica.port, "primary: answers");

    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("role: replica\n"), "{out}");
    assert!(out.contains("holds: history\n"), "{out}");
    let records = out
        .lines()
        .find_map(|line| line.strip_prefix("records: "))
        .expect("says its records");
    let (held, of) = records.split_once(" of the primary's ").expect("both");
    assert!(
        held.parse::<u64>().is_ok() && of.parse::<u64>().is_ok(),
        "{records}"
    );
    assert!(!out.contains("stopped"), "{out}");
}

#[test]
fn a_replica_that_keeps_current_values_only_says_so() {
    let dir = TempDir::new("current");
    dir.primary_and_replica();
    let primary = primary(&dir);
    let replica = replica(&dir, &primary, &["--current-values-only"]);

    let (code, out, err) = dir.status_with(replica.port, "primary: answers");

    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("holds: current values\n"), "{out}");
}

#[test]
fn a_replica_that_stopped_says_why_and_exits_one() {
    let dir = TempDir::new("stopped");
    dir.primary_and_replica();
    let primary = primary(&dir);
    let replica = replica(&dir, &primary, &[]);
    // A player the primary first heard of after the backup, on a link
    // that carries no keys: the replica cannot keep what it was sent.
    assert_eq!(
        first_line(primary.port, &["SET", "player/2/gems", "7"]),
        "+OK"
    );

    let (code, out, err) = dir.status_with(replica.port, "stopped: ");

    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("the key file lacks"), "{out}");
}

#[test]
fn a_replica_whose_primary_is_gone_says_so_and_exits_one() {
    let dir = TempDir::new("gone");
    dir.primary_and_replica();
    let primary = primary(&dir);
    let replica = replica(&dir, &primary, &[]);
    dir.status_with(replica.port, "primary: answers");

    drop(primary);
    let (code, out, err) = dir.status_with(replica.port, "primary: unreachable");

    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("role: replica\n"), "{out}");
}

#[test]
fn nothing_listening_is_an_error_and_no_log_is_made() {
    let dir = TempDir::new("nothing");
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        listener.local_addr().expect("has an address").port()
    };

    let (code, out, err) = dir.status(port);

    assert_eq!(code, 2, "{out}{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
    assert!(!dir.0.join("no-such-log").exists(), "a log was made");
    assert!(
        !dir.0.join("no-such-log.keys").exists(),
        "a key file was made"
    );
}

#[test]
fn a_server_that_does_not_know_the_command_is_an_error() {
    let dir = TempDir::new("other");
    let port = answers(b"-ERR unknown command 'UMBRA.REPLICATION'\r\n");

    let (code, out, err) = dir.status(port);

    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("unknown command"), "{err}");
}

#[test]
fn what_the_server_says_is_printed_without_its_control_characters() {
    let dir = TempDir::new("control");
    let port = answers(
        b"$162\r\n{\"role\":\"replica\",\"holds\":\"history\",\"feeds_replicas\":false,\
\"encrypted\":false,\"records\":null,\"primary_records\":null,\"unreachable\":null,\
\"stopped\":\"a\\u001b[31mb\\nc\"}\r\n",
    );

    let (code, out, err) = dir.status(port);

    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("stopped: a[31mbc\n"), "{out:?}");
    assert!(
        out.contains("records: not answered by its primary yet\n"),
        "{out}"
    );
}

#[test]
fn an_answer_that_is_not_a_status_is_an_error() {
    let dir = TempDir::new("not-status");
    let port = answers(b"+PONG\r\n");

    let (code, out, err) = dir.status(port);

    assert_eq!(code, 2, "{out}{err}");
    assert!(out.is_empty(), "{out}");
}

#[test]
fn a_primary_that_refuses_writes_says_so_and_exits_one_until_a_replica_holds_everything() {
    let dir = TempDir::new("refuses");
    dir.primary_and_replica();
    let options = ["--feed-replicas-on", "0", "--wait-for-replica", "0.05"];
    let primary = serve(&dir, "primary", &options);
    // No replica asks yet: the write is in the primary's log only.
    let only_here = first_line(primary.port, &["SET", "player/1/gems", "70"]);
    assert!(only_here.starts_with("-NOTREPLICATED "), "{only_here}");

    let (code, out, err) = dir.status(primary.port);

    assert_eq!(code, 1, "{out}{err}");
    assert!(
        out.contains("writes: refused until a replica holds everything\n"),
        "{out}"
    );
    assert!(out.contains("yes; a replica holds 0 of "), "{out}");

    let _replica = replica(&dir, &primary, &[]);
    let start = Instant::now();
    let (code, out) = loop {
        let (code, out, _) = dir.status(primary.port);
        if code == 0 {
            break (code, out);
        }
        assert!(start.elapsed() < LIMIT, "still refuses: {out}");
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(code, 0);
    let held = out
        .lines()
        .find_map(|line| line.rsplit_once("a replica holds "))
        .expect("says what a replica holds")
        .1;
    let (held, of) = held.split_once(" of ").expect("both");
    assert_eq!(Some(held), of.strip_suffix(" records"), "{out}");
    assert!(!out.contains("writes: refused"), "{out}");
}

#[test]
fn bind_is_refused_and_host_is_named() {
    let dir = TempDir::new("bind");

    let output = dir
        .umbrachyx("no-such-log", &["replication", "--bind", "10.0.0.5"])
        .output()
        .expect("runs");
    let (code, out, err) = said(&output);

    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("--host"), "{err}");
}

#[test]
fn a_server_that_never_answers_is_given_up_on() {
    let dir = TempDir::new("silent");
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener.local_addr().expect("has an address").port();
    // Takes the connection and says nothing; kept so it stays open.
    let _held = thread::spawn(move || {
        let held: Vec<TcpStream> = listener.incoming().flatten().collect();
        drop(held);
    });

    let start = Instant::now();
    let (code, out, err) = dir.status(port);

    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("no answer within"), "{err}");
    assert!(start.elapsed() < Duration::from_secs(20), "waited too long");
}

/// As `primary_and_replica`, with a player erased on the primary after
/// its replica was restored: the third record of the primary's log, and
/// one the replica does not hold yet.
fn primary_that_erased_and_replica(dir: &TempDir) {
    dir.primary_and_replica();
    let mut erase = vec!["erase", "player/1"];
    erase.extend_from_slice(&WHO);
    dir.run("primary", &erase);
}

#[test]
fn a_primary_says_where_its_last_erasure_is_and_that_the_replica_asking_holds_it() {
    let dir = TempDir::new("erasure");
    primary_that_erased_and_replica(&dir);
    let primary = primary(&dir);
    let (_, before, _) = dir.status(primary.port);
    let replica = replica(&dir, &primary, &[]);

    let wanted = "replicas asking now: 1, 0 of them holding fewer records than the last erasure\n";
    let (code, out, err) = dir.status_with(primary.port, wanted);
    let (_, of_the_replica, _) = dir.status_with(replica.port, "records: 3 of the primary's 3\n");

    assert_eq!(code, 0, "{out}{err}");
    assert!(
        before.contains("replicas asking now: 0, 0 of them holding"),
        "{before}"
    );
    let erasure = "last erasure: within its first 3 records\n";
    assert!(out.contains(erasure), "{out}");
    // The replica read the record as it arrived, and says the same.
    assert!(of_the_replica.contains(erasure), "{of_the_replica}");
    assert!(
        !of_the_replica.contains("replicas asking"),
        "{of_the_replica}"
    );

    drop(replica);
    dir.status_with(primary.port, "replicas asking now: 0, ");
}

#[test]
fn a_server_that_erased_nothing_says_so_and_counts_who_asks() {
    let dir = TempDir::new("no-erasure");
    dir.primary_and_replica();
    let primary = primary(&dir);
    let _replica = replica(&dir, &primary, &[]);

    let (code, out, err) = dir.status_with(primary.port, "replicas asking now: 1\n");

    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("last erasure: none\n"), "{out}");
}

#[test]
fn an_ask_that_is_refused_is_not_a_replica_asking() {
    let dir = TempDir::new("refused-ask");
    primary_that_erased_and_replica(&dir);
    let primary = primary(&dir);
    let feed = primary.feed.as_deref().expect("feeds");
    let mut stranger = TcpStream::connect(feed).expect("connects");
    // An ask in the right shape, of a log this server does not hold.
    let (log, chain) = ("0".repeat(32), "0".repeat(64));
    let ask = format!(
        "*5\r\n$15\r\nUMBRA.LOG.AFTER\r\n$32\r\n{log}\r\n$1\r\n0\r\n$64\r\n{chain}\r\n$4\r\n4096\r\n"
    );
    stranger.write_all(ask.as_bytes()).expect("sends");
    let mut answer = String::new();
    BufReader::new(&stranger)
        .read_line(&mut answer)
        .expect("reads");

    let (_, out, _) = dir.status(primary.port);

    assert!(answer.starts_with("-ANOTHERLOG"), "{answer}");
    assert!(out.contains("replicas asking now: 0, "), "{out}");
}
