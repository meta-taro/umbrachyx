//! What the `umbrachyx` binary tells its memory allocator when it starts.
//!
//! The setting is a name the allocator looks up in the finished program:
//! spelled wrongly, or dropped when the program is linked, everything
//! still builds and runs, only without it. So this asks the allocator
//! itself what it was told, by having it print its settings as the
//! process ends.
//!
//! Linux only: the setting is about Linux's transparent huge pages.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

/// The variable jemalloc reads its settings from, as this build names it.
const ALLOCATOR_ENV: &str = "_RJEM_MALLOC_CONF";

/// Runs a short command and returns what the allocator printed about its
/// settings when the process ended. `extra` is added to the settings the
/// way a person running the server would add it.
fn allocator_settings(name: &str, extra: &str) -> String {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("umbrachyx-allocator-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creates temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_umbrachyx"))
        .args(["branch", "app", "--from", "main"])
        .env("UMBRACHYX_DB", dir.join("log"))
        .env(ALLOCATOR_ENV, format!("stats_print:true{extra}"))
        .output()
        .expect("runs");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(output.status.success(), "the command failed: {output:?}");
    String::from_utf8(output.stderr).expect("the settings are text")
}

#[test]
fn the_binary_asks_for_huge_pages() {
    let settings = allocator_settings("default", "");

    assert!(settings.contains("opt.thp: \"always\""), "{settings}");
    assert!(
        settings.contains("opt.metadata_thp: \"always\""),
        "{settings}"
    );
}

#[test]
fn the_environment_turns_huge_pages_off() {
    let settings = allocator_settings("off", ",thp:default,metadata_thp:disabled");

    assert!(settings.contains("opt.thp: \"default\""), "{settings}");
    assert!(
        settings.contains("opt.metadata_thp: \"disabled\""),
        "{settings}"
    );
}
