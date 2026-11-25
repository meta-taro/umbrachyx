//! `umbrachyx backup <directory> --check`: says whether the backup in a
//! directory would restore, without restoring it, opening a store or
//! writing anything. The data is synthetic.

use std::path::PathBuf;
use std::process::{Command, Output};

const WHO: [&str; 4] = ["--who", "test", "--why", "a synthetic player"];

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-cli-check-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn path(&self, name: &str) -> String {
        self.0.join(name).to_str().expect("utf-8 path").to_owned()
    }

    fn run(&self, db: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_umbrachyx"))
            .args(args)
            .env("UMBRACHYX_DB", self.0.join(db))
            .output()
            .expect("runs")
    }

    /// A store with two commits, and a backup of it in `backup`.
    fn backed_up(&self) {
        for change in ["player/1/gems=55", "player/2/gems=7"] {
            let mut commit = vec!["commit", "main", change];
            commit.extend_from_slice(&WHO);
            assert!(self.run("store", &commit).status.success());
        }
        let backup = self.run("store", &["backup", &self.path("backup")]);
        assert!(backup.status.success(), "{}", err(&backup));
    }

    /// Checks the backup, with a store named that does not exist.
    fn check(&self) -> Output {
        self.run("no-such-log", &["backup", &self.path("backup"), "--check"])
    }

    /// Every file of the backup and what it holds, in order of name.
    fn backup_files(&self) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(self.0.join("backup"))
            .expect("lists")
            .map(|entry| entry.expect("an entry"))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, std::fs::read(entry.path()).expect("reads"))
            })
            .collect();
        files.sort();
        files
    }

    /// Changes one byte of the backup's file whose name starts with
    /// `prefix`, `from_end` bytes before its end.
    fn damage(&self, prefix: &str, from_end: usize) {
        let (name, mut bytes) = self
            .backup_files()
            .into_iter()
            .find(|(name, _)| name.starts_with(prefix))
            .expect("the backup has the file");
        let at = bytes.len() - from_end;
        bytes[at] ^= 0x01;
        std::fs::write(self.0.join("backup").join(name), bytes).expect("writes");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_backup_that_would_restore_checks_out_and_nothing_is_written() {
    let dir = TempDir::new("good");
    dir.backed_up();
    let before = dir.backup_files();

    let checked = dir.check();

    assert_eq!(checked.status.code(), Some(0), "{}", err(&checked));
    let said = out(&checked);
    assert!(said.contains("2 records"), "{said}");
    assert!(said.contains("would restore"), "{said}");
    assert_eq!(dir.backup_files(), before, "the backup was changed");
    assert!(!dir.0.join("no-such-log").exists(), "a log was made");
    assert!(
        !dir.0.join("no-such-log.keys").exists(),
        "a key file was made"
    );
}

#[test]
fn a_backup_checks_out_while_its_store_is_named() {
    let dir = TempDir::new("named");
    dir.backed_up();
    let store = std::fs::read(dir.0.join("store")).expect("reads");

    let checked = dir.run("store", &["backup", &dir.path("backup"), "--check"]);

    assert_eq!(checked.status.code(), Some(0), "{}", err(&checked));
    // Checked, not brought up to date: the store was not read.
    assert_eq!(std::fs::read(dir.0.join("store")).expect("reads"), store);
    assert!(out(&checked).contains("would restore"), "{}", out(&checked));
}

#[test]
fn a_place_with_no_backup_does_not_check_out_and_is_not_made() {
    let dir = TempDir::new("none");

    let checked = dir.check();

    assert_eq!(checked.status.code(), Some(2), "{}", out(&checked));
    assert!(
        err(&checked).contains("no finished backup"),
        "{}",
        err(&checked)
    );
    assert!(!dir.0.join("backup").exists(), "the place was made");
}

#[test]
fn a_changed_byte_in_the_copy_of_the_log_does_not_check_out() {
    let dir = TempDir::new("log");
    dir.backed_up();
    dir.damage("log", 9);
    let before = dir.backup_files();

    let checked = dir.check();

    assert_eq!(checked.status.code(), Some(2), "{}", out(&checked));
    assert!(out(&checked).is_empty(), "{}", out(&checked));
    assert_eq!(dir.backup_files(), before, "the backup was changed");
}

#[test]
fn a_changed_byte_in_the_key_file_does_not_check_out() {
    let dir = TempDir::new("keys");
    dir.backed_up();
    dir.damage("keys-", 1);

    let checked = dir.check();

    assert_eq!(checked.status.code(), Some(2), "{}", out(&checked));
    assert!(err(&checked).contains("key file"), "{}", err(&checked));
}

#[test]
fn a_backup_whose_restore_is_refused_for_the_place_still_checks_out() {
    let dir = TempDir::new("place");
    dir.backed_up();

    // Restoring over the store it was taken from is refused; the backup
    // itself is good, and that is what is asked.
    let restore = dir.run("store", &["restore", &dir.path("backup")]);
    let checked = dir.check();

    assert_eq!(restore.status.code(), Some(2));
    assert_eq!(checked.status.code(), Some(0), "{}", err(&checked));
}
