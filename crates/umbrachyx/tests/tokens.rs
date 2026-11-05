//! The command-line tool's key and token commands, run as a separate
//! process the way a person would run them, and `umbrachyx mcp` started
//! with a token the way an agent's host would start it.
//!
//! Every key and token here is made for the test in a temporary directory
//! and removed with it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const SUBJECT: &str = "agent:restock";
const BRANCH: &str = "agent/restock";
const PURPOSE: &str = "restock event";

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-tokens-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn umbrachyx(args: &[&str], stdin: &str) -> Output {
    umbrachyx_on(None, args, stdin)
}

/// Runs against the log at `db`, or with no log for commands that need none.
fn umbrachyx_on(db: Option<&Path>, args: &[&str], stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_umbrachyx"));
    if let Some(db) = db {
        command.env("UMBRACHYX_DB", db);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("starts");
    child
        .stdin
        .take()
        .expect("has stdin")
        .write_all(stdin.as_bytes())
        .expect("writes stdin");
    child.wait_with_output().expect("runs")
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .expect("prints text")
        .trim()
        .to_owned()
}

/// Makes a key and returns its public half.
fn keygen(key: &Path) -> String {
    stdout(&umbrachyx(&["keygen", key.to_str().expect("utf-8")], ""))
}

fn issue(key: &Path, branch: &str) -> Output {
    umbrachyx(
        &[
            "token",
            "issue",
            "--key",
            key.to_str().expect("utf-8"),
            "--subject",
            SUBJECT,
            "--branch",
            branch,
            "--prefix",
            "player/",
            "--columns",
            "gems,item",
            "--operations",
            "read,write",
            "--purpose",
            PURPOSE,
            "--expires-in",
            "8h",
        ],
        "",
    )
}

#[test]
fn keygen_writes_the_private_key_and_prints_the_public_key() {
    let dir = TempDir::new("keygen");
    let key = dir.path("root.key");

    let public = keygen(&key);

    let written = std::fs::read_to_string(&key).expect("reads the key");
    assert!(written.starts_with("ed25519-private/"), "{written}");
    assert!(public.starts_with("ed25519/"), "{public}");
    assert!(!public.contains(written.trim()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).expect("stats").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn keygen_never_overwrites_a_key() {
    let dir = TempDir::new("overwrite");
    let key = dir.path("root.key");
    keygen(&key);
    let before = std::fs::read(&key).expect("reads");

    let again = umbrachyx(&["keygen", key.to_str().expect("utf-8")], "");

    assert_eq!(again.status.code(), Some(2));
    assert_eq!(std::fs::read(&key).expect("reads"), before);
}

#[test]
fn an_issued_token_shows_its_holder() {
    let dir = TempDir::new("issue");
    let key = dir.path("root.key");
    let public = keygen(&key);

    let token = stdout(&issue(&key, BRANCH));
    let shown = stdout(&umbrachyx(
        &["token", "show", "--public-key", &public],
        &token,
    ));

    assert_eq!(
        shown,
        format!("subject: {SUBJECT}\nbranch: {BRANCH}\npurpose: {PURPOSE}")
    );
}

#[test]
fn a_narrowed_token_keeps_its_holder_and_another_root_refuses_it() {
    let dir = TempDir::new("narrow");
    let key = dir.path("root.key");
    let public = keygen(&key);
    let other = keygen(&dir.path("other.key"));
    let token = stdout(&issue(&key, BRANCH));

    let narrowed = stdout(&umbrachyx(
        &[
            "token",
            "narrow",
            "--public-key",
            &public,
            "--operation",
            "read",
            "--prefix",
            "player/42/",
            "--expires-in",
            "30m",
        ],
        &token,
    ));
    let shown = stdout(&umbrachyx(
        &["token", "show", "--public-key", &public],
        &narrowed,
    ));
    let foreign = umbrachyx(&["token", "show", "--public-key", &other], &narrowed);

    assert_ne!(narrowed, token);
    assert!(
        shown.starts_with(&format!("subject: {SUBJECT}\n")),
        "{shown}"
    );
    assert_eq!(foreign.status.code(), Some(2));
}

#[test]
fn a_token_that_would_write_to_main_is_not_issued() {
    let dir = TempDir::new("main");
    let key = dir.path("root.key");
    keygen(&key);

    let output = issue(&key, "main");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn an_agent_reads_through_umbrachyx_mcp_within_its_token() {
    let dir = TempDir::new("mcp");
    let key = dir.path("root.key");
    let db = dir.path("log");
    let public = keygen(&key);
    stdout(&umbrachyx_on(
        Some(&db),
        &[
            "commit",
            "main",
            "player/42/gems=120",
            "player/7/gems=30",
            "--who",
            "game:onboarding",
            "--why",
            "starter grant",
        ],
        "",
    ));
    let token_file = dir.path("agent.token");
    std::fs::write(&token_file, stdout(&issue(&key, BRANCH))).expect("writes the token");
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get","arguments":{"key":"player/42/gems"}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get","arguments":{"key":"player/7/email"}}}"#,
    ];

    let output = umbrachyx_on(
        Some(&db),
        &[
            "mcp",
            "--public-key",
            &public,
            "--token-file",
            token_file.to_str().expect("utf-8"),
        ],
        &(requests.join("\n") + "\n"),
    );

    let replies = stdout(&output);
    let replies: Vec<&str> = replies.lines().collect();
    assert_eq!(replies.len(), 3, "{replies:?}");
    assert!(replies[0].contains(r#""protocolVersion":"2025-06-18""#));
    assert!(
        replies[1].contains(r#"\"value\":\"120\""#),
        "{}",
        replies[1]
    );
    assert!(replies[2].contains(r#""isError":true"#), "{}", replies[2]);
}

#[test]
fn umbrachyx_mcp_does_not_start_with_a_token_from_another_root() {
    let dir = TempDir::new("mcp-foreign");
    let key = dir.path("root.key");
    keygen(&key);
    let other = keygen(&dir.path("other.key"));
    let token_file = dir.path("agent.token");
    std::fs::write(&token_file, stdout(&issue(&key, BRANCH))).expect("writes the token");

    let output = umbrachyx_on(
        Some(&dir.path("log")),
        &[
            "mcp",
            "--public-key",
            &other,
            "--token-file",
            token_file.to_str().expect("utf-8"),
        ],
        "",
    );

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("the token is not valid"), "{stderr}");
}
