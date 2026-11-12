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

#[test]
fn serve_with_require_auth_refuses_writes_without_auth() {
    let dir = TempDir::new("require-auth");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let key = dir.0.join("root.key");
    let keygen = dir.run(&["keygen", key.to_str().expect("utf-8")]);
    assert!(keygen.status.success());
    let public = String::from_utf8(keygen.stdout)
        .expect("text")
        .trim()
        .to_owned();
    let port = free_port();
    let port_arg = port.to_string();
    let _server = Server(
        dir.umbrachyx(&[
            "serve",
            BRANCH,
            "--port",
            &port_arg,
            "--public-key",
            &public,
            "--require-auth",
        ])
        .spawn()
        .expect("starts"),
    );

    let mut client = connect(port);
    client
        .write_all(b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\nb\r\n")
        .expect("sends");
    let expected = b"-NOAUTH Authentication required.\r\n";
    let mut reply = vec![0; expected.len()];
    client.read_exact(&mut reply).expect("reads");
    assert_eq!(reply, expected);
}

#[test]
fn serve_refuses_require_auth_without_a_public_key() {
    let dir = TempDir::new("require-auth-no-key");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    let output = dir.run(&["serve", BRANCH, "--require-auth"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--public-key"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The server reserves its log ahead of what it writes, so a sync has only
/// the record to make durable. The command-line tool reads such a log after
/// the server is gone, and the server takes it up again.
#[test]
fn serve_reserves_its_log_and_other_commands_still_read_it() {
    const RESERVE: u64 = 16 * 1024 * 1024;
    let dir = TempDir::new("reserve");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    let set = |port: u16, key: &str, value: &str| {
        let mut client = connect(port);
        let command = format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n${}\r\n{value}\r\n",
            key.len(),
            value.len()
        );
        client.write_all(command.as_bytes()).expect("sends");
        let mut reply = [0; 5];
        client.read_exact(&mut reply).expect("reads");
        assert_eq!(&reply, b"+OK\r\n");
    };
    let serve = |port: u16| {
        let port_arg = port.to_string();
        Server(
            dir.umbrachyx(&["serve", BRANCH, "--port", &port_arg])
                .spawn()
                .expect("starts"),
        )
    };

    let port = free_port();
    let server = serve(port);
    set(port, "a", "1");
    let reserved = std::fs::metadata(dir.0.join("log")).expect("stats").len();
    drop(server);
    let read = dir.run(&["get", BRANCH, "a"]);
    let port = free_port();
    let server = serve(port);
    set(port, "b", "2");
    drop(server);

    assert_eq!(reserved, RESERVE);
    assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), "1");
    let both = [
        dir.run(&["get", BRANCH, "a"]),
        dir.run(&["get", BRANCH, "b"]),
    ];
    assert_eq!(String::from_utf8_lossy(&both[0].stdout).trim(), "1");
    assert_eq!(String::from_utf8_lossy(&both[1].stdout).trim(), "2");
}

#[test]
fn export_prints_the_log_as_one_json_record_a_line() {
    let dir = TempDir::new("export");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    assert!(
        dir.run(&[
            "commit",
            BRANCH,
            "player/1/gems=40",
            "--who",
            "agent-7",
            "--why",
            "refund",
        ])
        .status
        .success()
    );

    let output = dir.run(&["export"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["record"], "branch");
    assert_eq!(lines[1]["who"], "agent-7");
    assert_eq!(lines[1]["changes"][0]["key"], "player/1/gems");
    assert_eq!(lines[1]["changes"][0]["value"], "40");
    // The log is as it was: the next command reads the same value.
    let read = dir.run(&["get", BRANCH, "player/1/gems"]);
    assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), "40");
}

#[test]
fn serve_takes_a_number_of_serving_threads_and_refuses_what_is_not_one() {
    let dir = TempDir::new("threads");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    for bad in ["0", "-1", "many"] {
        let output = dir.run(&["serve", BRANCH, "--threads", bad]);
        assert!(!output.status.success(), "{bad}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("--threads {bad:?} is not a positive number")),
            "{bad}: {stderr}"
        );
    }
    let port = free_port();
    let port_arg = port.to_string();
    let _server = Server(
        dir.umbrachyx(&["serve", BRANCH, "--port", &port_arg, "--threads", "1"])
            .spawn()
            .expect("starts"),
    );

    // One thread serves both connections.
    let mut first = connect(port);
    let mut second = connect(port);
    for stream in [&mut first, &mut second] {
        stream.write_all(b"*1\r\n$4\r\nPING\r\n").expect("writes");
        let mut reply = [0; 7];
        stream.read_exact(&mut reply).expect("reads");
        assert_eq!(&reply, b"+PONG\r\n");
    }
}
