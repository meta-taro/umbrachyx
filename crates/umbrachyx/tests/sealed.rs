//! A store opened with a key file keeps every value encrypted under its
//! subject's key: the log holds no value in the clear, and everything read
//! back, from a value to a merge conflict, is the value that was written.
//! The data is synthetic.

use std::path::{Path, PathBuf};

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{
    Committer, Conflict, MAIN_BRANCH, Merged, Provenance, Reverted, Store, StoreError, SystemClock,
    ValueChange,
};

type SealedStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-sealed-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self) -> PathBuf {
        self.0.join("log")
    }

    fn keys(&self) -> PathBuf {
        self.0.join("log.keys")
    }

    fn open(&self) -> Result<SealedStore, StoreError> {
        open(&self.log(), &self.keys())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(log: &Path, keys: &Path) -> Result<SealedStore, StoreError> {
    let storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(keys));
    let keys = Keys::open(storage).expect("opens the key file");
    Store::open_sealed(
        FileStorage::open(log).expect("opens file"),
        keys,
        SystemClock,
    )
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

fn by(who: &str, why: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
        why: why.to_owned(),
        approved_by: None,
    }
}

fn text(value: Option<impl AsRef<[u8]>>) -> Option<String> {
    value.map(|v| String::from_utf8(v.as_ref().to_vec()).expect("utf-8"))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn the_log_holds_no_value_in_the_clear() {
    let dir = TempDir::new("clear");
    let mut store = dir.open().expect("opens");
    store
        .commit(
            MAIN_BRANCH,
            vec![
                set("player/42/email", "p42-secret@example.test"),
                set("player/7/email", "p7-secret@example.test"),
            ],
            by("game:signup", "new players"),
        )
        .expect("commits");

    let read = text(store.get(MAIN_BRANCH, b"player/42/email"));
    drop(store);
    let log = std::fs::read(dir.log()).expect("reads the log");
    let reopened = dir.open().expect("reopens");

    assert_eq!(read.as_deref(), Some("p42-secret@example.test"));
    assert!(!contains(&log, b"secret@example.test"));
    // Keys stay readable: they are how history is found.
    assert!(contains(&log, b"player/42/email"));
    assert_eq!(
        text(reopened.get(MAIN_BRANCH, b"player/7/email")).as_deref(),
        Some("p7-secret@example.test")
    );
}

#[test]
fn history_diff_and_old_values_read_as_written() {
    let dir = TempDir::new("history");
    let mut store = dir.open().expect("opens");
    let first = store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "120")],
            by("a", "grant"),
        )
        .expect("commits");
    let second = store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("b", "potion"),
        )
        .expect("commits");

    let history: Vec<Option<String>> = store
        .history(MAIN_BRANCH, b"player/42/gems")
        .into_iter()
        .map(|cell| text(cell.value))
        .collect();
    let diff = store.diff(Some(&first), Some(&second)).expect("diffs");
    let old = text(store.get_at(&first, b"player/42/gems").expect("reads"));
    let entries: Vec<(String, String)> = store
        .entries(MAIN_BRANCH)
        .into_iter()
        .map(|(k, v)| (text(Some(k)).expect("key"), text(Some(v)).expect("value")))
        .collect();

    assert_eq!(history, [Some("95".to_owned()), Some("120".to_owned())]);
    assert_eq!(
        diff,
        [ValueChange {
            key: b"player/42/gems"[..].into(),
            before: Some(b"120"[..].into()),
            after: Some(b"95"[..].into()),
            erased: false,
        }]
    );
    assert_eq!(old.as_deref(), Some("120"));
    assert_eq!(entries, [("player/42/gems".to_owned(), "95".to_owned())]);
}

#[test]
fn merges_and_reverts_work_and_their_conflicts_read_as_written() {
    let dir = TempDir::new("merge");
    let mut store = dir.open().expect("opens");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("a", "start"),
        )
        .expect("commits");
    store.create_branch("event", MAIN_BRANCH).expect("branches");
    // The same change on both sides is taken once, which needs equal values
    // to stay equal once encrypted.
    store
        .commit(
            "event",
            vec![set("player/42/item", "cape"), set("player/7/gems", "1")],
            by("e", "event"),
        )
        .expect("commits");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/item", "cape")],
            by("m", "shop"),
        )
        .expect("commits");
    let merged = store
        .merge(MAIN_BRANCH, "event", by("ops", "merge"))
        .expect("merges");
    store
        .commit(
            "event",
            vec![set("player/42/gems", "5000")],
            by("e", "reward"),
        )
        .expect("commits");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "90")],
            by("m", "spent"),
        )
        .expect("commits");
    let conflict = store
        .merge(MAIN_BRANCH, "event", by("ops", "merge"))
        .expect("merges");
    let shop = store.head(MAIN_BRANCH).expect("has a head");
    let reverted = store
        .revert(MAIN_BRANCH, &shop, by("ops", "undo"))
        .expect("reverts");

    assert!(matches!(merged, Merged::Commit(_)), "{merged:?}");
    assert_eq!(
        text(store.get(MAIN_BRANCH, b"player/7/gems")).as_deref(),
        Some("1")
    );
    assert_eq!(
        conflict,
        Merged::Conflicts(vec![Conflict {
            key: b"player/42/gems"[..].into(),
            base: Some(b"95"[..].into()),
            ours: Some(b"90"[..].into()),
            theirs: Some(b"5000"[..].into()),
        }])
    );
    assert!(matches!(reverted, Reverted::Commit(_)), "{reverted:?}");
    assert_eq!(
        text(store.get(MAIN_BRANCH, b"player/42/gems")).as_deref(),
        Some("95")
    );
}

#[test]
fn readers_on_other_threads_read_as_written() {
    let dir = TempDir::new("view");
    let committer = Committer::start(dir.open().expect("opens"));

    committer
        .writer()
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("a", "grant"),
        )
        .expect("commits");
    let view = committer.reader().view();

    assert_eq!(
        text(view.get(MAIN_BRANCH, b"player/42/gems")).as_deref(),
        Some("95")
    );
}

/// A key the log needs and the key file lacks is refused when opening,
/// before anything is written that could cut the key file.
#[test]
fn a_missing_key_file_is_refused() {
    let dir = TempDir::new("missing");
    let mut store = dir.open().expect("opens");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("a", "grant"),
        )
        .expect("commits");
    drop(store);
    std::fs::rename(dir.keys(), dir.0.join("elsewhere")).expect("moves the key file");

    let opened = dir.open();

    assert!(
        matches!(opened, Err(StoreError::MissingKeys(1))),
        "{:?}",
        opened.err()
    );
}

#[test]
fn a_log_written_without_encryption_is_refused() {
    let dir = TempDir::new("plain");
    let mut plain = Store::open(
        FileStorage::open(&dir.log()).expect("opens file"),
        SystemClock,
    )
    .expect("opens");
    plain
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("a", "grant"),
        )
        .expect("commits");
    drop(plain);

    let opened = dir.open();

    assert!(
        matches!(opened, Err(StoreError::NotSealed)),
        "{:?}",
        opened.err()
    );
}

/// Damage in the last key reads like a torn end, since no key follows it.
/// If the log uses that key, opening refuses before the key file is cut.
#[test]
fn a_damaged_last_key_the_log_uses_is_refused() {
    let dir = TempDir::new("damaged-last");
    let mut store = dir.open().expect("opens");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("a", "grant"),
        )
        .expect("commits");
    drop(store);
    let mut bytes = std::fs::read(dir.keys()).expect("reads the key file");
    let last = bytes.len() - 10;
    bytes[last] ^= 1;
    std::fs::write(dir.keys(), &bytes).expect("writes the key file");

    let opened = dir.open();

    assert!(
        matches!(opened, Err(StoreError::MissingKeys(1))),
        "{:?}",
        opened.err()
    );
    assert_eq!(std::fs::read(dir.keys()).expect("reads"), bytes);
}

/// A key file that counts the writes that make keys durable.
struct CountKeyWrites {
    file: FileKeyStorage,
    writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl KeyStorage for CountKeyWrites {
    fn read_all(&mut self) -> std::io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.file.append(bytes)
    }

    fn replace(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.file.replace(bytes)
    }
}

/// Commits written together that bring many new subjects make all their
/// keys durable with one write of the key file, as they share one sync of
/// the log: a wave of new players is not a sync each.
#[test]
fn commits_written_together_make_their_new_keys_with_one_write() {
    use umbrachyx::store::CommitRequest;
    let dir = TempDir::new("keys-together");
    let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let storage: Box<dyn KeyStorage + Send> = Box::new(CountKeyWrites {
        file: FileKeyStorage::new(&dir.keys()),
        writes: std::sync::Arc::clone(&writes),
    });
    let mut store = Store::open_sealed(
        FileStorage::open(&dir.log()).expect("opens file"),
        Keys::open(storage).expect("opens the key file"),
        SystemClock,
    )
    .expect("opens");
    let requests = (0..40)
        .map(|n| CommitRequest {
            branch: MAIN_BRANCH.to_owned(),
            edits: vec![
                set(&format!("player/{n}/gems"), "10"),
                set(&format!("player/{n}/item"), "sword"),
            ],
            provenance: by("game:signup", "new player"),
        })
        .collect();

    let results = store.commit_all(requests).expect("writes the group");

    assert!(results.iter().all(Result::is_ok));
    assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(store);
    let reopened = dir.open().expect("reopens");
    assert_eq!(
        text(reopened.get(MAIN_BRANCH, b"player/39/item")).as_deref(),
        Some("sword")
    );
}
