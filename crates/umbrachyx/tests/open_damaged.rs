//! A log that cannot be opened is left as it was found: opening reads the
//! records one at a time, and only once every one has decoded does it cut
//! a torn tail or write anything else. The data is synthetic.

use std::io::Write;
use std::path::PathBuf;

use umbrachyx::log::{Edit, FileStorage, LogError};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, StoreError, SystemClock};

/// A fresh file per test, removed when the test ends.
struct TempLog(PathBuf);

impl TempLog {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "umbrachyx-open-damaged-{}-{name}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn open(&self) -> Result<Store<FileStorage, SystemClock>, StoreError> {
        Store::open(FileStorage::open(&self.0).expect("opens file"), SystemClock)
    }

    /// A log with two commits.
    fn written(name: &str) -> Self {
        let log = Self::new(name);
        let mut store = log.open().expect("opens");
        for value in ["100", "150"] {
            let edit = Edit {
                key: b"player/1/gems".to_vec(),
                value: Some(value.as_bytes().to_vec()),
            };
            let provenance = Provenance {
                who: "seed".to_owned(),
                why: "test data".to_owned(),
                approved_by: None,
            };
            store
                .commit(MAIN_BRANCH, vec![edit], provenance)
                .expect("commits");
        }
        log
    }

    fn append(&self, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.0)
            .expect("opens file");
        file.write_all(bytes).expect("appends");
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.0).expect("reads file")
    }
}

impl Drop for TempLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A frame written whole, its checksum right, that no version of this
/// code decodes: a length, a checksum, and a payload of a kind unknown.
fn frame_that_does_not_decode() -> Vec<u8> {
    let payload = [0xff_u8; 32];
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

#[test]
fn a_torn_tail_is_cut_and_the_records_before_it_are_there() {
    let log = TempLog::written("torn");
    let whole = log.bytes();
    log.append(b"half a record, cut by a crash");

    let store = log.open().expect("opens past the torn tail");

    assert_eq!(
        store.get(MAIN_BRANCH, b"player/1/gems").as_deref(),
        Some(&b"150"[..])
    );
    drop(store);
    assert_eq!(log.bytes(), whole);
}

#[test]
fn a_log_with_a_record_that_does_not_decode_is_refused_and_left_as_it_was() {
    let log = TempLog::written("undecodable");
    log.append(&frame_that_does_not_decode());
    log.append(b"and a torn tail after it");
    let found = log.bytes();

    let refused = log.open();

    assert!(
        matches!(refused, Err(StoreError::Log(LogError::Corrupt { .. }))),
        "{:?}",
        refused.err()
    );
    // Not even the torn tail was cut: nothing of a log that is refused
    // is written.
    assert_eq!(log.bytes(), found);
}
