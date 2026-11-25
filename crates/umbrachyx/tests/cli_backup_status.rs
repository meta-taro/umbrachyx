//! `umbrachyx backup --status`: asks a running server how its backup is
//! going and prints it, without opening any log and without saying where
//! the backup is kept. The data is synthetic.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BRANCH: &str = "app";
const LIMIT: Duration = Duration::from_secs(30);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-cli-backup-status-{}-{name}",
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

    /// Starts `serve` on the store `store`, new but for its branch, on a
    /// port the system chooses, and returns it with that port.
    fn serve(&self, options: &[&str]) -> (Server, u16) {
        let made = self
            .umbrachyx("store", &["branch", BRANCH, "--from", "main"])
            .output()
            .expect("runs");
        assert!(made.status.success());
        let (server, port, _) = self.start("store", options);
        (server, port)
    }

    /// Starts `serve` on the store `db` and returns it with its port and
    /// what it said on starting.
    fn start(&self, db: &str, options: &[&str]) -> (Server, u16, String) {
        let mut args = vec!["serve", BRANCH, "--port", "0"];
        args.extend_from_slice(options);
        let mut child = self
            .umbrachyx(db, &args)
            .stderr(Stdio::piped())
            .spawn()
            .expect("starts");
        let mut stderr = BufReader::new(child.stderr.take().expect("piped"));
        let server = Server(child);
        let mut all = String::new();
        let port = loop {
            let mut line = String::new();
            let read = stderr.read_line(&mut line).expect("reads");
            assert!(read > 0, "the server ended, having said: {all}");
            all.push_str(&line);
            if line.starts_with("serving ") {
                let (_, port) = line.trim_end().rsplit_once(':').expect("an address");
                break port.parse().expect("a port");
            }
        };
        // Whatever it says later is read and dropped, so it never blocks.
        thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = stderr.read_to_end(&mut rest);
        });
        (server, port, all)
    }

    /// What `umbrachyx backup --status` says of the server on `port`:
    /// its exit status, what it printed, and what it complained of.
    fn status(&self, port: u16) -> (i32, String, String) {
        let output = self
            .umbrachyx(
                "no-such-log",
                &["backup", "--status", "--port", &port.to_string()],
            )
            .output()
            .expect("runs");
        said(&output)
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
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn set(port: u16, key: &str, value: &str) {
    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    let command = format!(
        "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{value}\r\n",
        key.len(),
        value.len()
    );
    client.write_all(command.as_bytes()).expect("sends");
    let mut reply = [0; 5];
    client.read_exact(&mut reply).expect("reads");
    assert_eq!(&reply, b"+OK\r\n");
}

#[test]
fn a_server_that_keeps_a_backup_says_how_far_it_is_and_not_where() {
    let dir = TempDir::new("kept");
    let place = dir.path("backup");
    let (_server, port) = dir.serve(&["--backup-to", &place, "--backup-every", "0.05"]);
    set(port, "player/1/gems", "55");

    let (code, out, err) = dir.status_with(port, " s ago\n");

    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("keeps a backup: yes\n"), "{out}");
    assert!(out.contains("failing: no\n"), "{out}");
    let records = out
        .lines()
        .find_map(|line| line.strip_prefix("records: "))
        .expect("says its records");
    let (held, of) = records.split_once(" of the store's ").expect("both");
    assert!(
        held.parse::<u64>().is_ok() && of.parse::<u64>().is_ok(),
        "{records}"
    );
    assert!(!out.contains(&place), "{out}");
    assert!(!dir.0.join("no-such-log").exists(), "a log was made");
}

#[test]
fn a_server_that_keeps_no_backup_says_so_and_exits_zero() {
    let dir = TempDir::new("none");
    let (_server, port) = dir.serve(&[]);

    let (code, out, err) = dir.status(port);

    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(out, "keeps a backup: no\n");
}

#[test]
fn a_server_whose_backups_fail_says_so_and_exits_one() {
    let dir = TempDir::new("failing");
    let place = dir.path("backup");
    let (_server, port) = dir.serve(&["--backup-to", &place, "--backup-every", "0.05"]);
    set(port, "player/1/gems", "55");
    dir.status_with(port, " s ago\n");

    // The place goes, as when its disk does, and something is written.
    // A backup may be writing a file into it at this moment, and the
    // place is then not empty when it is removed: until it is gone.
    let started = std::time::Instant::now();
    while let Err(error) = std::fs::remove_dir_all(&place) {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "removes the backup: {error}"
        );
    }
    set(port, "player/1/gems", "70");
    let (code, out, err) = dir.status_with(port, "failing: yes");

    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("standard error"), "{out}");
    assert!(!out.contains(&place), "{out}");
}

#[test]
fn status_takes_no_directory_and_goes_with_backup_only() {
    let dir = TempDir::new("usage");

    let with_a_place = dir
        .umbrachyx("no-such-log", &["backup", &dir.path("backup"), "--status"])
        .output()
        .expect("runs");
    let elsewhere = dir
        .umbrachyx("no-such-log", &["restore", &dir.path("backup"), "--status"])
        .output()
        .expect("runs");

    let both = dir
        .umbrachyx("no-such-log", &["backup", "--status", "--check"])
        .output()
        .expect("runs");
    assert_eq!(said(&both).0, 2);
    assert!(said(&both).2.contains("one at a time"), "{:?}", said(&both));
    assert_eq!(said(&with_a_place).0, 2);
    assert_eq!(said(&elsewhere).0, 2);
    assert!(
        said(&elsewhere).2.contains("backup --status"),
        "{:?}",
        said(&elsewhere)
    );
    assert!(!dir.0.join("no-such-log").exists(), "a log was made");
    assert!(!dir.0.join("backup").exists(), "a backup was made");
}

#[test]
fn nothing_listening_is_an_error() {
    let dir = TempDir::new("nothing");
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        listener.local_addr().expect("has an address").port()
    };

    let (code, out, err) = dir.status(port);

    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
}

#[test]
fn a_replica_that_keeps_a_backup_counts_the_records_it_was_sent() {
    let dir = TempDir::new("replica");
    let run = |db: &str, args: &[&str]| {
        let output = dir.umbrachyx(db, args).output().expect("runs");
        assert!(output.status.success(), "{args:?}: {:?}", said(&output));
    };
    run("store", &["branch", BRANCH, "--from", "main"]);
    let who = ["--who", "test", "--why", "a synthetic player"];
    run(
        "store",
        &[&["commit", BRANCH, "player/1/gems=55"][..], &who[..]].concat(),
    );
    run("store", &["backup", &dir.path("seed")]);
    run("replica", &["restore", &dir.path("seed")]);
    let (_primary, primary_port, said) = dir.start("store", &["--feed-replicas-on", "0"]);
    let feed = said
        .lines()
        .find_map(|line| line.strip_prefix("handing the log to replicas that ask on "))
        .expect("says where replicas ask");
    let place = dir.path("replica-backup");
    let options = [
        "--follow",
        feed,
        "--backup-to",
        &place,
        "--backup-every",
        "0.05",
    ];
    let (_replica, replica_port, _) = dir.start("replica", &options);

    // The third record of the log: the replica takes no write itself.
    set(primary_port, "player/1/gems", "70");
    let (code, out, err) = dir.status_with(replica_port, "records: 3 of the store's 3\n");

    assert_eq!(code, 0, "{out}{err}");
}
