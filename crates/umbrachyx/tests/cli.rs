//! The command-line tool run as separate processes: `serve` options, and a
//! second process on a log already in use.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const BRANCH: &str = "bench";
/// How long the server gets to start listening.
const START_TRIES: usize = 200;
const START_WAIT: Duration = Duration::from_millis(25);
/// How long a second process may take to be told the log is in use. It
/// should be told at once; this only has to be longer than starting it.
const REFUSAL_LIMIT: Duration = Duration::from_secs(5);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("umbrachyx-cli-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn umbrachyx(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_umbrachyx"));
        command.args(args).env("UMBRACHYX_DB", self.0.join("log"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.umbrachyx(args).output().expect("runs")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Stops the server when the test ends, passed or not.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
    listener.local_addr().expect("has an address").port()
}

fn connect(port: u16) -> TcpStream {
    for _ in 0..START_TRIES {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            return stream;
        }
        thread::sleep(START_WAIT);
    }
    panic!("the server did not start listening on {port}");
}

#[test]
fn serve_turns_away_connections_past_max_connections() {
    let dir = TempDir::new("max-connections");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let port = free_port();
    let port_arg = port.to_string();
    let _server = Server(
        dir.umbrachyx(&[
            "serve",
            BRANCH,
            "--port",
            &port_arg,
            "--max-connections",
            "1",
        ])
        .spawn()
        .expect("starts"),
    );

    let mut first = connect(port);
    first.write_all(b"*1\r\n$4\r\nPING\r\n").expect("sends");
    let mut pong = [0; 7];
    first.read_exact(&mut pong).expect("reads");
    assert_eq!(&pong, b"+PONG\r\n");

    let mut second = connect(port);
    let mut reply = Vec::new();
    second.read_to_end(&mut reply).expect("reads until closed");
    assert_eq!(reply, b"-ERR max number of clients reached\r\n");
}

#[test]
fn serve_refuses_a_max_connections_that_is_not_a_positive_number() {
    let dir = TempDir::new("bad-max-connections");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    for bad in ["0", "-1", "many"] {
        let output = dir.run(&["serve", BRANCH, "--max-connections", bad]);
        assert!(!output.status.success(), "{bad}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "--max-connections {bad:?} is not a positive number"
            )),
            "{bad}: {stderr}"
        );
    }
}

#[test]
fn a_second_process_on_a_log_in_use_is_refused_at_once() {
    let dir = TempDir::new("log-in-use");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let port = free_port();
    let port_arg = port.to_string();
    let _server = Server(
        dir.umbrachyx(&["serve", BRANCH, "--port", &port_arg])
            .spawn()
            .expect("starts"),
    );
    drop(connect(port));

    let started = Instant::now();
    let mut second = dir
        .umbrachyx(&["get", "config"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("starts");
    while second.try_wait().expect("polls").is_none() {
        if started.elapsed() > REFUSAL_LIMIT {
            let _ = second.kill();
            panic!("a second process waited on the log instead of being refused");
        }
        thread::sleep(START_WAIT);
    }
    let output = second.wait_with_output().expect("finishes");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already open"), "stderr: {stderr}");
}
