//! Declaring how a table is held from the command line, and what `history`
//! and `diff` print for a table that keeps only its latest values. Run as
//! a separate process. The data is synthetic: player positions and gold.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-cli-tables-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_umbrachyx"))
            .args(args)
            .env("UMBRACHYX_DB", self.0.join("log"))
            .output()
            .expect("runs")
    }

    /// Runs and expects success; returns what was printed.
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("prints text")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const WHO: [&str; 4] = ["--who", "ops", "--why", "positions change every tick"];

fn declare(dir: &TempDir, table: &str, engine: &str) -> Output {
    let mut args = vec!["table", table, engine];
    args.extend(WHO);
    dir.run(&args)
}

#[test]
fn a_declared_table_is_listed_with_its_engine() {
    let dir = TempDir::new("declare");

    assert_eq!(dir.ok(&["tables"]), "", "nothing declared yet");
    assert!(declare(&dir, "pos", "latest").status.success());

    assert_eq!(dir.ok(&["tables"]), "pos  latest\n");
}

#[test]
fn a_table_cannot_be_declared_with_an_unknown_engine_or_once_written() {
    let dir = TempDir::new("refused");

    let unknown = declare(&dir, "pos", "append");
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("history or latest"),
        "{}",
        String::from_utf8_lossy(&unknown.stderr)
    );

    let mut commit = vec!["commit", "main", "pos/1=3,4"];
    commit.extend(WHO);
    dir.ok(&commit);
    let in_use = declare(&dir, "pos", "latest");
    assert_eq!(in_use.status.code(), Some(2));
    assert_eq!(dir.ok(&["tables"]), "", "nothing was declared");
}

#[test]
fn history_of_a_latest_key_says_the_table_keeps_none() {
    let dir = TempDir::new("history");
    assert!(declare(&dir, "pos", "latest").status.success());
    let mut commit = vec!["commit", "main", "pos/1=3,4"];
    commit.extend(WHO);
    dir.ok(&commit);

    let output = dir.run(&["history", "main", "pos/1"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("table 'pos' keeps only the latest value"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(dir.ok(&["get", "main", "pos/1"]), "3,4\n");
}

#[test]
fn a_diff_into_the_past_of_main_names_the_table_it_left_out() {
    let dir = TempDir::new("diff");
    assert!(declare(&dir, "pos", "latest").status.success());
    for (pos, gold) in [("1,1", "1"), ("2,2", "2")] {
        let (pos, gold) = (format!("pos/1={pos}"), format!("player/1/gold={gold}"));
        let mut commit = vec!["commit", "main", &pos, &gold];
        commit.extend(WHO);
        dir.ok(&commit);
    }

    let output = dir.run(&["diff", "main~1", "main"]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "~ player/1/gold: 1 -> 2\n",
        "stdout holds only changes"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "(left out: pos keeps only its latest values, and main keeps no past for it)\n"
    );
}
