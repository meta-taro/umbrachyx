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

/// Starts `serve` with `extra` options and returns it with what it says
/// on starting, read up to the line that names where it listens.
fn serve_and_its_first_words(dir: &TempDir, extra: &[&str]) -> (Server, String) {
    let (server, said, _) = serve_on_a_free_port(dir, extra);
    (server, said)
}

/// As `serve_and_its_first_words`, with the port it listens on.
fn serve_on_a_free_port(dir: &TempDir, extra: &[&str]) -> (Server, String, u16) {
    use std::io::{BufRead, BufReader};

    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );
    // Port 0: the system gives the server a free one, which it says.
    // Naming a port here first would let two tests name the same.
    let mut args = vec!["serve", BRANCH, "--port", "0"];
    args.extend_from_slice(extra);
    let mut child = dir
        .umbrachyx(&args)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("starts");
    let stderr = BufReader::new(child.stderr.take().expect("stderr is piped"));
    let server = Server(child);
    let mut said = String::new();
    let mut port = None;
    for line in stderr.lines() {
        let line = line.expect("reads stderr");
        said.push_str(&line);
        said.push('\n');
        if line.starts_with("serving ") {
            port = line
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok());
            break;
        }
    }
    let port = port.unwrap_or_else(|| panic!("the server did not say where it serves: {said}"));
    let said = said.replace(&format!(":{port}"), ":PORT");
    (server, said, port)
}

#[test]
fn serve_listens_only_on_this_machine_unless_told_otherwise() {
    let dir = TempDir::new("bind-default");

    let (_server, said) = serve_and_its_first_words(&dir, &[]);

    assert_eq!(said, "serving bench on 127.0.0.1:PORT\n");
}

#[test]
fn serve_with_bind_listens_there_and_says_what_that_opens() {
    let dir = TempDir::new("bind-all");

    let (_server, said) = serve_and_its_first_words(&dir, &["--bind", "0.0.0.0"]);

    assert_eq!(
        said,
        "umbrachyx: listening beyond this machine: anyone who can reach the port \
         reads every value and its history without a token, and nothing is encrypted\n\
         serving bench on 0.0.0.0:PORT\n"
    );
}

#[test]
fn serve_with_bind_on_this_machine_gives_no_warning() {
    let dir = TempDir::new("bind-loopback");

    let (_server, said) = serve_and_its_first_words(&dir, &["--bind", "127.0.0.1"]);

    assert_eq!(said, "serving bench on 127.0.0.1:PORT\n");
}

#[test]
fn serve_refuses_a_bind_that_is_not_an_address() {
    let dir = TempDir::new("bind-bad");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    let output = dir.run(&["serve", BRANCH, "--bind", "everywhere"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--bind \"everywhere\" is not an address"),
        "stderr: {stderr}"
    );
}

fn commit_gems(dir: &TempDir, value: &str) {
    let key = format!("player/1/gems={value}");
    let output = dir.run(&["commit", "main", &key, "--who", "seed", "--why", "test"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs a command on the store at `db` in the test's directory.
fn run_on(dir: &TempDir, db: &str, args: &[&str]) -> Output {
    dir.umbrachyx(args)
        .env("UMBRACHYX_DB", dir.0.join(db))
        .output()
        .expect("runs")
}

#[test]
fn backup_and_restore_bring_a_store_back_somewhere_else() {
    let dir = TempDir::new("backup");
    let backup = dir.0.join("backup");
    let backup = backup.to_str().expect("utf-8 path");
    commit_gems(&dir, "40");

    let first = dir.run(&["backup", backup]);
    commit_gems(&dir, "55");
    let second = dir.run(&["backup", backup]);
    let restored = run_on(&dir, "restored", &["restore", backup]);

    for output in [&first, &second, &restored] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "stderr: {stderr}");
    }
    let said = String::from_utf8_lossy(&second.stdout);
    assert!(
        said.contains("2 records") && said.contains("1 new"),
        "{said}"
    );
    let read = run_on(&dir, "restored", &["get", "main", "player/1/gems"]);
    assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), "55");
    let history = |db: &str| run_on(&dir, db, &["log", "main"]).stdout;
    assert_eq!(history("restored"), history("log"));
}

#[test]
fn restore_refuses_to_write_over_a_store_and_leaves_nothing_where_it_fails() {
    let dir = TempDir::new("restore-refused");
    let backup = dir.0.join("backup");
    let backup = backup.to_str().expect("utf-8 path");
    commit_gems(&dir, "40");
    assert!(dir.run(&["backup", backup]).status.success());

    let over = dir.run(&["restore", backup]);
    let empty = dir.0.join("empty");
    std::fs::create_dir_all(&empty).expect("creates");
    let from_nothing = run_on(
        &dir,
        "nowhere",
        &["restore", empty.to_str().expect("utf-8")],
    );

    assert_eq!(over.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&over.stderr).contains("exists"));
    assert_eq!(from_nothing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&from_nothing.stderr).contains("no finished backup"));
    assert!(!dir.0.join("nowhere").exists());
    assert_eq!(std::fs::read_dir(&empty).expect("lists").count(), 0);
    assert!(!dir.0.join("nowhere.keys").exists());
    // The store that was there reads as before.
    let read = dir.run(&["get", "main", "player/1/gems"]);
    assert_eq!(String::from_utf8_lossy(&read.stdout).trim(), "40");
}

#[test]
fn serve_with_backup_to_keeps_a_backup_that_restores_what_clients_wrote() {
    let dir = TempDir::new("serve-backup");
    let backup = dir.0.join("backup");
    let backup = backup.to_str().expect("utf-8 path");
    let options = ["--backup-to", backup, "--backup-every", "0.05"];

    let (_server, said, port) = serve_on_a_free_port(&dir, &options);
    let mut client = connect(port);
    client
        .write_all(b"*3\r\n$3\r\nSET\r\n$13\r\nplayer/1/gems\r\n$2\r\n55\r\n")
        .expect("sends");
    let mut reply = [0; 5];
    client.read_exact(&mut reply).expect("reads");
    assert_eq!(&reply, b"+OK\r\n");

    assert!(
        said.contains(&format!("backing up to {backup} every 0.05 s")),
        "{said}"
    );
    let start = Instant::now();
    loop {
        let _ = std::fs::remove_file(dir.0.join("restored"));
        let _ = std::fs::remove_file(dir.0.join("restored.keys"));
        let restored = run_on(&dir, "restored", &["restore", backup]);
        let read = run_on(&dir, "restored", &["get", BRANCH, "player/1/gems"]);
        if restored.status.success() && String::from_utf8_lossy(&read.stdout).trim() == "55" {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the backup never held what was written: {}",
            String::from_utf8_lossy(&restored.stderr)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serve_refuses_to_start_where_its_backup_cannot_be_kept() {
    let dir = TempDir::new("serve-backup-refused");
    let backup = dir.0.join("backup");
    let backup = backup.to_str().expect("utf-8 path");
    // The place holds the backup of another store.
    let other = run_on(&dir, "other", &["backup", backup]);
    assert!(other.status.success());
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    let another = dir.run(&["serve", BRANCH, "--backup-to", backup]);
    let not_a_time = dir.run(&[
        "serve",
        BRANCH,
        "--backup-to",
        backup,
        "--backup-every",
        "0",
    ]);
    let nowhere = dir.run(&["serve", BRANCH, "--backup-every", "5"]);

    for output in [&another, &not_a_time, &nowhere] {
        assert_eq!(output.status.code(), Some(2));
    }
    assert!(String::from_utf8_lossy(&another.stderr).contains("another log"));
    assert!(String::from_utf8_lossy(&not_a_time.stderr).contains("--backup-every"));
    assert!(String::from_utf8_lossy(&nowhere.stderr).contains("--backup-to"));
}

/// Sends one command and returns the first line of its reply.
fn first_line(client: &mut TcpStream, command: &[&str]) -> String {
    use std::io::{BufRead, BufReader};

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

/// Asks again until the reply to `command` starts with `wanted`.
fn wait_for(client: &mut TcpStream, command: &[&str], wanted: &str) -> String {
    let start = Instant::now();
    loop {
        let reply = first_line(client, command);
        if reply.contains(wanted) {
            return reply;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{command:?} was never answered with {wanted:?}, but {reply:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn serve_with_follow_answers_reads_from_a_restored_backup_of_the_server_it_follows() {
    let dir = TempDir::new("serve-follow");
    let backup = dir.0.join("backup");
    let backup = backup.to_str().expect("utf-8 path");
    let feed_port = free_port().to_string();
    let options = [
        "--feed-replicas-on",
        &feed_port,
        "--backup-to",
        backup,
        "--backup-every",
        "0.05",
    ];
    let (_primary, said, port) = serve_on_a_free_port(&dir, &options);
    assert!(
        said.contains(&format!(
            "handing the log to replicas that ask on 127.0.0.1:{feed_port}"
        )),
        "{said}"
    );
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
        let restored = run_on(&dir, "replica", &["restore", backup]);
        let read = run_on(&dir, "replica", &["get", BRANCH, "player/1/gems"]);
        if restored.status.success() && String::from_utf8_lossy(&read.stdout).trim() == "55" {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "no backup");
        thread::sleep(Duration::from_millis(50));
    }
    let replica_port = free_port();
    let follow = format!("127.0.0.1:{feed_port}");
    let _replica = Server(
        dir.umbrachyx(&[
            "serve",
            BRANCH,
            "--port",
            &replica_port.to_string(),
            "--follow",
            &follow,
        ])
        .env("UMBRACHYX_DB", dir.0.join("replica"))
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("starts"),
    );
    let mut replica = connect(replica_port);

    assert_eq!(
        first_line(&mut primary, &["SET", "player/1/gems", "70"]),
        "+OK"
    );

    wait_for(&mut replica, &["GET", "player/1/gems"], "70");
    let refused = first_line(&mut replica, &["SET", "player/1/gems", "1"]);
    assert!(refused.starts_with("-READONLY "), "{refused}");
    // A player the primary first heard of after the backup: the key the
    // values are sealed under was never given to the replica.
    assert_eq!(
        first_line(&mut primary, &["SET", "player/2/gems", "7"]),
        "+OK"
    );
    let said = wait_for(&mut replica, &["UMBRA.REPLICATION"], "the key file lacks");
    assert!(said.contains("\"role\":\"replica\""), "{said}");
    assert_eq!(first_line(&mut replica, &["GET", "player/1/gems"]), "70");
}

#[test]
fn serve_that_follows_can_keep_current_values_only_and_says_so_when_asked_about_the_past() {
    let dir = TempDir::new("serve-current");
    let backup = dir.0.join("backup");
    let backup = backup.to_str().expect("utf-8 path");
    let feed_port = free_port().to_string();
    let options = [
        "--feed-replicas-on",
        &feed_port,
        "--backup-to",
        backup,
        "--backup-every",
        "0.05",
    ];
    let (_primary, _, port) = serve_on_a_free_port(&dir, &options);
    let mut primary = connect(port);
    assert_eq!(
        first_line(&mut primary, &["SET", "player/1/gems", "55"]),
        "+OK"
    );
    let start = Instant::now();
    loop {
        let _ = std::fs::remove_file(dir.0.join("replica"));
        let _ = std::fs::remove_file(dir.0.join("replica.keys"));
        let restored = run_on(&dir, "replica", &["restore", backup]);
        let read = run_on(&dir, "replica", &["get", BRANCH, "player/1/gems"]);
        if restored.status.success() && String::from_utf8_lossy(&read.stdout).trim() == "55" {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "no backup");
        thread::sleep(Duration::from_millis(50));
    }
    let replica_port = free_port();
    let follow = format!("127.0.0.1:{feed_port}");
    let _replica = Server(
        dir.umbrachyx(&[
            "serve",
            BRANCH,
            "--port",
            &replica_port.to_string(),
            "--follow",
            &follow,
            "--current-values-only",
        ])
        .env("UMBRACHYX_DB", dir.0.join("replica"))
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("starts"),
    );
    let mut replica = connect(replica_port);

    assert_eq!(
        first_line(&mut primary, &["SET", "player/1/gems", "70"]),
        "+OK"
    );

    wait_for(&mut replica, &["GET", "player/1/gems"], "70");
    let past = first_line(&mut replica, &["UMBRA.HISTORY", BRANCH, "player/1/gems"]);
    assert!(
        past.starts_with("-NOHISTORY this server keeps current values only"),
        "{past}"
    );
    let said = first_line(&mut replica, &["UMBRA.REPLICATION"]);
    assert!(said.contains("\"holds\":\"current values\""), "{said}");
}

#[test]
fn serve_keeps_current_values_only_for_a_server_that_follows() {
    let dir = TempDir::new("serve-current-refused");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    let alone = dir.run(&["serve", BRANCH, "--current-values-only"]);
    let another_command = dir.run(&["log", BRANCH, "--current-values-only"]);

    for refused in [&alone, &another_command] {
        assert!(!refused.status.success());
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(
            said.contains("is for a server that follows another"),
            "{said}"
        );
    }
}

#[test]
fn serve_refuses_to_follow_what_is_not_a_host_and_a_port() {
    let dir = TempDir::new("serve-follow-refused");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    let refused = dir.run(&["serve", BRANCH, "--follow", "the-primary"]);

    assert!(!refused.status.success());
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("is not a host and a port"), "{said}");
}

#[test]
fn serve_that_waits_for_a_replica_says_which_writes_no_replica_holds() {
    let dir = TempDir::new("serve-wait");
    let feed_port = free_port().to_string();
    let options = [
        "--feed-replicas-on",
        &feed_port,
        "--wait-for-replica",
        "0.05",
    ];
    let (_primary, said, port) = serve_on_a_free_port(&dir, &options);
    let mut primary = connect(port);

    // No replica ever asks.
    let only_here = first_line(&mut primary, &["SET", "player/1/gems", "55"]);
    let not_written = first_line(&mut primary, &["SET", "player/1/gems", "70"]);

    assert!(
        said.contains("answering a write only once a replica holds it, waiting 0.05 s at most"),
        "{said}"
    );
    assert!(only_here.starts_with("-NOTREPLICATED "), "{only_here}");
    assert!(not_written.starts_with("-NOREPLICA "), "{not_written}");
    assert_eq!(first_line(&mut primary, &["GET", "player/1/gems"]), "55");
    let status = first_line(&mut primary, &["UMBRA.REPLICATION"]);
    assert!(status.contains("\"refuses_writes\":true"), "{status}");
    assert!(status.contains("\"replica_records\":0"), "{status}");
}

#[test]
fn serve_refuses_to_wait_for_replicas_it_has_no_port_for() {
    let dir = TempDir::new("serve-wait-refused");
    assert!(
        dir.run(&["branch", BRANCH, "--from", "main"])
            .status
            .success()
    );

    let no_port = dir.run(&["serve", BRANCH, "--wait-for-replica", "5"]);
    let not_a_time = dir.run(&[
        "serve",
        BRANCH,
        "--feed-replicas-on",
        "1",
        "--wait-for-replica",
        "soon",
    ]);

    let said = String::from_utf8_lossy(&no_port.stderr);
    assert!(said.contains("needs --feed-replicas-on"), "{said}");
    let said = String::from_utf8_lossy(&not_a_time.stderr);
    assert!(said.contains("is not a number of seconds"), "{said}");
}
