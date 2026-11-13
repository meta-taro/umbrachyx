//! Runs every command in `docs/demo.md` and in each walkthrough under
//! `docs/examples/`, in order, and checks its output against the document,
//! so a walkthrough cannot drift from the code.
//!
//! In each `console` block, a line starting with `$ umbrachyx` is a command
//! and the lines after it, up to the next command, are its output. `{id}`
//! stands for a short commit id and `{time}` for a UTC time, which change on
//! every run. A command whose output starts with `conflict on` is one that
//! stops on conflicts; every other command has to succeed.

use std::path::PathBuf;
use std::process::Command;

const DEMO: &str = include_str!("../../../docs/demo.md");
/// Every walkthrough under `docs/examples/`, by file name.
const EXAMPLES: &[(&str, &str)] = &[
    (
        "game-inventory.md",
        include_str!("../../../docs/examples/game-inventory.md"),
    ),
    (
        "support-desk.md",
        include_str!("../../../docs/examples/support-desk.md"),
    ),
    (
        "staged-settings.md",
        include_str!("../../../docs/examples/staged-settings.md"),
    ),
];
/// The page that lists the walkthroughs; it has no commands of its own.
const EXAMPLES_INDEX: &str = "README.md";
/// What a command prints first when it stops on conflicts.
const CONFLICT: &str = "conflict on ";
/// The exit code of a command that stopped on conflicts.
const STOPPED_ON_CONFLICTS: i32 = 1;
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

/// Runs every command of `document` against a log of its own and checks
/// what each prints.
fn run_as_written(name: &str, document: &str, at_least: usize) {
    let dir = TempDir(std::env::temp_dir().join(format!(
        "umbrachyx-walkthrough-{}-{name}",
        std::process::id()
    )));
    std::fs::create_dir_all(&dir.0).expect("creates temp dir");
    let steps = steps(document);
    assert!(
        steps.len() >= at_least,
        "{name}: found only {} commands",
        steps.len()
    );

    for step in steps {
        let output = Command::new(env!("CARGO_BIN_EXE_umbrachyx"))
            .args(words(&step.command))
            .env("UMBRACHYX_DB", dir.0.join("walkthrough.log"))
            .output()
            .expect("runs the binary");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
        let actual: Vec<&str> = stdout.lines().collect();
        let stops_on_conflicts = step.output.iter().any(|line| line.starts_with(CONFLICT));
        let expected_code = if stops_on_conflicts {
            STOPPED_ON_CONFLICTS
        } else {
            0
        };
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{name}: `umbrachyx {}` printed:\n{stdout}{}",
            step.command,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            actual.len(),
            step.output.len(),
            "{name}: `umbrachyx {}` printed:\n{stdout}",
            step.command
        );
        for (expected, actual) in step.output.iter().zip(&actual) {
            assert!(
                matches(expected, actual),
                "{name}: `umbrachyx {}`\nexpected: {expected}\n  actual: {actual}",
                step.command
            );
        }
    }
}

#[test]
fn the_demo_document_runs_as_written() {
    run_as_written("demo.md", DEMO, 20);
}

#[test]
fn every_example_runs_as_written() {
    for (name, document) in EXAMPLES {
        run_as_written(name, document, 8);
    }
}

#[test]
fn every_file_under_examples_is_run() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("reads docs/examples")
        .map(|entry| {
            entry
                .expect("reads an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name != EXAMPLES_INDEX)
        .collect();
    on_disk.sort();
    let mut run: Vec<&str> = EXAMPLES.iter().map(|(name, _)| *name).collect();
    run.sort_unstable();

    assert_eq!(on_disk, run);
}

#[test]
fn every_example_shows_a_change_approved_by_someone() {
    for (name, document) in EXAMPLES {
        assert!(
            document.contains("--approved-by "),
            "{name} approves nothing"
        );
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
