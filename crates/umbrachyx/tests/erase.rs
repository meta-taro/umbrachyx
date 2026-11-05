//! Erasing a subject: "Delete my account." After it, the subject's values
//! can never be read again, while the history keeps its shape: which keys
//! changed, in which commits, when, by whom and why. Other subjects are
//! untouched, and the erasure itself is recorded. The data is synthetic.

use std::io;
use std::path::PathBuf;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock};

type SealedStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-erase-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self) -> PathBuf {
        self.0.join("log")
    }

    fn keys(&self) -> PathBuf {
        self.0.join("log.keys")
    }

    fn open(&self) -> SealedStore {
        self.open_with(Box::new(FileKeyStorage::new(&self.keys())))
    }

    fn open_with(&self, storage: Box<dyn KeyStorage + Send>) -> SealedStore {
        let keys = Keys::open(storage).expect("opens the key file");
        Store::open_sealed(
            FileStorage::open(&self.log()).expect("opens file"),
            keys,
            SystemClock,
        )
        .expect("opens")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A key file whose rewrites fail, as a crash in the middle of erasing
/// would leave it: the old file still in place.
struct FailingRewrites(FileKeyStorage);

impl KeyStorage for FailingRewrites {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.0.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.append(bytes)
    }

    fn replace(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::other("the disk went away"))
    }
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

/// Player 42 and player 7, with a few changes each.
fn seed(store: &mut SealedStore) {
    store
        .commit(
            MAIN_BRANCH,
            vec![
                set("player/42/gems", "120"),
                set("player/42/email", "p42@example.test"),
                set("player/7/gems", "30"),
            ],
            by("game:onboarding", "starter grant"),
        )
        .expect("commits");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("game:shop", "bought a potion"),
        )
        .expect("commits");
}

fn erase_player_42(store: &mut SealedStore) -> bool {
    store
        .erase(b"player/42", by("ops:privacy", "account deletion request"))
        .expect("erases")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Question 5 of the README, end to end, before and after reopening.
#[test]
fn an_erased_subject_cannot_be_read_but_its_history_keeps_its_shape() {
    let dir = TempDir::new("question-5");
    let mut store = dir.open();
    seed(&mut store);
    let before: Vec<_> = store
        .history(MAIN_BRANCH, b"player/42/gems")
        .into_iter()
        .map(|cell| cell.changed_by)
        .collect();

    let erased = erase_player_42(&mut store);
    let check = |store: &SealedStore| {
        let history = store.history(MAIN_BRANCH, b"player/42/gems");
        assert_eq!(store.get(MAIN_BRANCH, b"player/42/gems"), None);
        assert_eq!(store.get(MAIN_BRANCH, b"player/42/email"), None);
        let cell = store
            .cell(MAIN_BRANCH, b"player/42/gems")
            .expect("has a cell");
        assert!(cell.erased);
        // The shape: the same two changes, in the same commits, by the same
        // people, for the same reasons; only the values are gone.
        assert_eq!(
            history.iter().map(|c| c.changed_by).collect::<Vec<_>>(),
            before
        );
        assert!(
            history
                .iter()
                .all(|cell| cell.erased && cell.value.is_none())
        );
        let reasons: Vec<&str> = store
            .log(MAIN_BRANCH)
            .iter()
            .map(|info| info.provenance.why.as_str())
            .collect();
        assert_eq!(reasons, ["bought a potion", "starter grant"]);
        // Everyone else reads as before.
        assert_eq!(
            text(store.get(MAIN_BRANCH, b"player/7/gems")).as_deref(),
            Some("30")
        );
        assert!(
            !store
                .cell(MAIN_BRANCH, b"player/7/gems")
                .expect("has a cell")
                .erased
        );
    };

    assert!(erased);
    check(&store);
    drop(store);
    check(&dir.open());
    let keys = std::fs::read(dir.keys()).expect("reads the key file");
    assert!(!contains(&keys, b"player/42"));
}

#[test]
fn the_erasure_is_recorded_with_who_and_why() {
    let dir = TempDir::new("recorded");
    let mut store = dir.open();
    seed(&mut store);

    erase_player_42(&mut store);
    drop(store);
    let reopened = dir.open();

    let erasures = reopened.erasures();
    assert_eq!(erasures.len(), 1);
    assert_eq!(erasures[0].subject, b"player/42");
    assert_eq!(erasures[0].provenance.who, "ops:privacy");
    assert_eq!(erasures[0].provenance.why, "account deletion request");
}

#[test]
fn a_subject_with_no_key_has_nothing_to_erase() {
    let dir = TempDir::new("nothing");
    let mut store = dir.open();
    seed(&mut store);

    let erased = store
        .erase(b"player/99", by("ops:privacy", "unknown player"))
        .expect("erases nothing");

    assert!(!erased);
    assert!(store.erasures().is_empty());
}

#[test]
fn writes_after_erasing_use_a_new_key_and_the_old_values_stay_gone() {
    let dir = TempDir::new("after");
    let mut store = dir.open();
    seed(&mut store);
    erase_player_42(&mut store);

    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "10")],
            by("game:signup", "new account"),
        )
        .expect("commits");
    drop(store);
    let store = dir.open();

    let history = store.history(MAIN_BRANCH, b"player/42/gems");
    assert_eq!(text(history[0].value.as_ref()).as_deref(), Some("10"));
    assert!(history[1..].iter().all(|cell| cell.erased));
    assert_eq!(store.get(MAIN_BRANCH, b"player/42/email"), None);
}

/// The erasure is in the log before the key file is rewritten. If the
/// rewrite does not happen, the next open finishes it.
#[test]
fn an_erasure_cut_short_is_finished_when_the_store_opens() {
    let dir = TempDir::new("cut-short");
    let mut store = dir.open();
    seed(&mut store);
    drop(store);
    let mut store = dir.open_with(Box::new(FailingRewrites(FileKeyStorage::new(&dir.keys()))));

    let failed = store.erase(b"player/42", by("ops:privacy", "account deletion request"));
    drop(store);
    let reopened = dir.open();

    assert!(failed.is_err());
    assert_eq!(reopened.get(MAIN_BRANCH, b"player/42/gems"), None);
    assert_eq!(reopened.erasures().len(), 1);
    let keys = std::fs::read(dir.keys()).expect("reads the key file");
    assert!(!contains(&keys, b"player/42"));
    assert_eq!(
        text(reopened.get(MAIN_BRANCH, b"player/7/gems")).as_deref(),
        Some("30")
    );
}

#[test]
fn readers_on_other_threads_lose_the_values_at_once() {
    let dir = TempDir::new("view");
    let mut store = dir.open();
    seed(&mut store);
    let view = store.view();
    let before = text(view.get(MAIN_BRANCH, b"player/42/gems"));

    erase_player_42(&mut store);

    assert_eq!(before.as_deref(), Some("95"));
    assert_eq!(view.get(MAIN_BRANCH, b"player/42/gems"), None);
}

#[test]
fn a_diff_marks_erased_values_rather_than_showing_them_absent() {
    let dir = TempDir::new("diff");
    let mut store = dir.open();
    seed(&mut store);
    let first = store.log(MAIN_BRANCH).last().expect("has commits").id;
    let second = store.head(MAIN_BRANCH).expect("has a head");

    erase_player_42(&mut store);
    let diff = store.diff(Some(&first), Some(&second)).expect("diffs");

    assert_eq!(diff.len(), 1);
    assert_eq!(&*diff[0].key, b"player/42/gems");
    assert!(diff[0].erased);
    assert_eq!(
        (diff[0].before.clone(), diff[0].after.clone()),
        (None, None)
    );
}
