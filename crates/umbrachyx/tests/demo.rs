//! Runs every command in `docs/demo.md` in order and checks its output
//! against the document, so the walkthrough cannot drift from the code.
//!
//! In each `console` block, a line starting with `$ umbrachyx` is a command
//! and the lines after it, up to the next command, are its output. `{id}`
//! stands for a short commit id and `{time}` for a UTC time, which change on
//! every run.

use std::path::PathBuf;
use std::process::Command;

const DEMO: &str = include_str!("../../../docs/demo.md");
const PROMPT: &str = "$ umbrachyx ";
const ID_PLACEHOLDER: &str = "{id}";
const TIME_PLACEHOLDER: &str = "{time}";
/// A short commit id: 12 hex digits.
const ID_LEN: usize = 12;
/// `2026-11-02T12:34:56Z`.
const TIME_LEN: usize = 20;

struct Step {
    command: String,
    output: Vec<String>,
}

fn steps(document: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut in_console = false;
    for line in document.lines() {
        match line.trim_end() {
            "```console" => in_console = true,
            "```" => in_console = false,
            line if in_console => match line.strip_prefix(PROMPT) {
                Some(command) => steps.push(Step {
                    command: command.to_owned(),
                    output: Vec::new(),
                }),
                None => steps
                    .last_mut()
                    .expect("output follows a command")
                    .output
                    .push(line.to_owned()),
            },
            _ => {}
        }
    }
    steps
}

/// Splits like a shell does for the commands used here: on spaces, with
/// double quotes keeping a phrase together.
fn words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_word = false;
    for c in command.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_word = true;
            }
            ' ' if !in_quotes => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            c => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(current);
    }
    words
}

/// Whether `actual` matches `expected`, where placeholders stand for a
/// short id or a time.
fn matches(expected: &str, actual: &str) -> bool {
    let next = [ID_PLACEHOLDER, TIME_PLACEHOLDER]
        .into_iter()
        .filter_map(|p| expected.find(p).map(|i| (i, p)))
        .min();
    let Some((start, placeholder)) = next else {
        return expected == actual;
    };
    let len = if placeholder == ID_PLACEHOLDER {
        ID_LEN
    } else {
        TIME_LEN
    };
    let (Some(prefix), Some(value)) = (actual.get(..start), actual.get(start..start + len)) else {
        return false;
    };
    let is_valid = if placeholder == ID_PLACEHOLDER {
        value.bytes().all(|b| b.is_ascii_hexdigit())
    } else {
        is_time(value)
    };
    prefix == &expected[..start]
        && is_valid
        && matches(
            &expected[start + placeholder.len()..],
            &actual[start + len..],
        )
}

fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == TIME_LEN
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z'
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_demo_document_runs_as_written() {
    let dir = TempDir(std::env::temp_dir().join(format!("umbrachyx-demo-{}", std::process::id())));
    std::fs::create_dir_all(&dir.0).expect("creates temp dir");
    let steps = steps(DEMO);
    assert!(steps.len() >= 20, "found only {} commands", steps.len());

    for step in steps {
        let output = Command::new(env!("CARGO_BIN_EXE_umbrachyx"))
            .args(words(&step.command))
            .env("UMBRACHYX_DB", dir.0.join("demo.log"))
            .output()
            .expect("runs the binary");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
        let actual: Vec<&str> = stdout.lines().collect();
        assert!(
            output.status.success(),
            "`umbrachyx {}` failed: {}",
            step.command,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            actual.len(),
            step.output.len(),
            "`umbrachyx {}` printed:\n{stdout}",
            step.command
        );
        for (expected, actual) in step.output.iter().zip(&actual) {
            assert!(
                matches(expected, actual),
                "`umbrachyx {}`\nexpected: {expected}\n  actual: {actual}",
                step.command
            );
        }
    }
}

#[test]
fn placeholders_match_only_what_they_stand_for() {
    assert!(matches(
        "committed {id} to main",
        "committed 0123456789ab to main"
    ));
    assert!(!matches(
        "committed {id} to main",
        "committed 0123456789 to main"
    ));
    assert!(!matches(
        "committed {id} to main",
        "committed 0123456789xy to main"
    ));
    assert!(matches(
        "{time}  {id}  x",
        "2026-11-02T12:34:56Z  0123456789ab  x"
    ));
    assert!(!matches("{time}  x", "2026-11-02 12:34:56Z  x"));
    assert!(!matches("a", "b"));
}

#[test]
fn quoted_phrases_stay_one_word() {
    assert_eq!(
        words(r#"commit main a=1 --why "two words""#),
        vec!["commit", "main", "a=1", "--why", "two words"]
    );
}
