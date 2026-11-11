//! The log written out as JSON Lines: one record a line, in the order the
//! log holds them, with values opened under the key file. The data is
//! synthetic.

use std::path::PathBuf;

use serde_json::{Value, json};
use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::store::{CommitRequest, MAIN_BRANCH, Provenance, Store, SystemClock, hex};

type SealedStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-export-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn keys(&self) -> Keys<Box<dyn KeyStorage + Send>> {
        let storage: Box<dyn KeyStorage + Send> =
            Box::new(FileKeyStorage::new(&self.0.join("log.keys")));
        Keys::open(storage).expect("opens the key file")
    }

    fn storage(&self) -> FileStorage {
        FileStorage::open(&self.0.join("log")).expect("opens file")
    }

    fn open(&self) -> SealedStore {
        Store::open_sealed(self.storage(), self.keys(), SystemClock).expect("opens")
    }

    fn export(&self) -> Vec<Value> {
        SealedStore::export(self.storage(), self.keys(), SystemClock)
            .expect("exports")
            .iter()
            .map(|line| {
                assert!(!line.contains('\n'), "one record a line: {line}");
                serde_json::from_str(line).expect("each line is JSON")
            })
            .collect()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn set(key: &str, value: &[u8]) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.to_vec()),
    }
}

fn by(who: &str, why: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
        why: why.to_owned(),
        approved_by: None,
    }
}

#[test]
fn every_record_is_one_line_in_log_order_with_its_values_readable() {
    let dir = TempDir::new("records");
    let (first, second) = {
        let mut store = dir.open();
        store
            .declare_table(
                b"session",
                Engine::Latest,
                by("ops", "sessions keep no past"),
            )
            .expect("declares");
        store.create_branch("agent", MAIN_BRANCH).expect("branches");
        let first = store
            .commit(
                "agent",
                vec![
                    set("player/1/gems", b"40"),
                    set("player/1/icon", &[0xff, 0x00]),
                ],
                by("agent-7", "refund"),
            )
            .expect("commits");
        let second = store
            .commit(
                "agent",
                vec![Edit {
                    key: b"player/1/icon".to_vec(),
                    value: None,
                }],
                by("agent-7", "icon removed"),
            )
            .expect("commits");
        store
            .reject("agent", &second, by("reviewer", "wrong player"))
            .expect("rejects");
        (first, second)
    };

    let lines = dir.export();

    let kinds: Vec<&str> = lines
        .iter()
        .map(|line| line["record"].as_str().expect("names its kind"))
        .collect();
    assert_eq!(kinds, ["table", "branch", "commit", "commit", "reject"]);
    assert_eq!(lines[0]["name"], "session");
    assert_eq!(lines[0]["engine"], "latest");
    assert_eq!(lines[0]["who"], "ops");
    assert_eq!(lines[1]["name"], "agent");
    assert_eq!(lines[1]["from"], MAIN_BRANCH);
    assert_eq!(lines[2]["commit"], hex(&first));
    assert_eq!(lines[2]["parent"], Value::Null);
    assert_eq!(lines[2]["branch"], "agent");
    assert_eq!(lines[2]["who"], "agent-7");
    assert_eq!(lines[2]["why"], "refund");
    assert_eq!(lines[2]["approved_by"], json!([]));
    assert!(lines[2]["time_us"].as_u64().expect("has a time") > 0);
    assert_eq!(
        lines[2]["changes"],
        json!([
            {"key": "player/1/gems", "value": "40", "erased": false},
            {"key": "player/1/icon", "value": {"hex": "ff00"}, "erased": false},
        ])
    );
    assert_eq!(lines[3]["commit"], hex(&second));
    assert_eq!(lines[3]["parent"], hex(&first));
    assert_eq!(
        lines[3]["changes"],
        json!([{"key": "player/1/icon", "value": null, "erased": false}])
    );
    assert_eq!(lines[4]["branch"], "agent");
    assert_eq!(lines[4]["head"], hex(&second));
    assert_eq!(lines[4]["why"], "wrong player");
}

#[test]
fn an_erased_subjects_values_are_marked_erased_and_not_shown() {
    let dir = TempDir::new("erased");
    {
        let mut store = dir.open();
        store
            .commit(
                MAIN_BRANCH,
                vec![set("player/1/name", b"synthetic-one")],
                by("seed", "fill"),
            )
            .expect("commits");
        store
            .commit(
                MAIN_BRANCH,
                vec![set("player/2/name", b"synthetic-two")],
                by("seed", "fill"),
            )
            .expect("commits");
        assert!(
            store
                .erase(b"player/1", by("ops", "asked to be forgotten"))
                .expect("erases")
        );
    }

    let lines = dir.export();

    assert_eq!(
        lines[0]["changes"],
        json!([{"key": "player/1/name", "value": null, "erased": true}])
    );
    assert_eq!(
        lines[1]["changes"],
        json!([{"key": "player/2/name", "value": "synthetic-two", "erased": false}])
    );
    assert_eq!(lines[2]["record"], "erase");
    assert_eq!(lines[2]["subject"], "player/1");
    assert_eq!(lines[2]["why"], "asked to be forgotten");
    assert!(
        !lines
            .iter()
            .any(|line| line.to_string().contains("synthetic-one"))
    );
}

#[test]
fn commits_written_together_are_a_line_each() {
    let dir = TempDir::new("group");
    let ids: Vec<_> = {
        let mut store = dir.open();
        store
            .commit_all(vec![
                CommitRequest {
                    branch: MAIN_BRANCH.to_owned(),
                    edits: vec![set("a/1/x", b"1")],
                    provenance: by("seed", "one"),
                },
                CommitRequest {
                    branch: MAIN_BRANCH.to_owned(),
                    edits: vec![set("a/1/x", b"2")],
                    provenance: by("seed", "two"),
                },
            ])
            .expect("writes")
            .into_iter()
            .map(|id| id.expect("commits"))
            .collect()
    };

    let lines = dir.export();

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["commit"], hex(&ids[0]));
    assert_eq!(lines[1]["commit"], hex(&ids[1]));
    assert_eq!(lines[1]["parent"], hex(&ids[0]));
    assert_eq!(lines[1]["why"], "two");
}
