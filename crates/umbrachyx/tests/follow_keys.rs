//! Keys that travel from a primary to its replica: over a link that
//! carries them, a replica is given the key of a subject first written
//! after it was made, checks that it is the key the records name, and
//! keeps it before it keeps the records. Over a link that carries none,
//! and for a key its primary has destroyed, it stops where it is. The
//! data is synthetic.

use std::path::PathBuf;

use umbrachyx::keys::{FileKeyStorage, Fingerprint, KEY_LEN, KeyStorage, Keys};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::backup::{DirSource, DirTarget, restore};
use umbrachyx::store::follow::{
    Ask, FollowError, Follower, KeyAnswer, MAX_SENT, MAX_THROUGH, Primary, Refusal, Sent, WithKeys,
    follow_once,
};
use umbrachyx::store::{
    CommitId, Committer, MAIN_BRANCH, Provenance, Store, StoreError, SystemClock,
};

type FileStore = Store<FileStorage, SystemClock>;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "umbrachyx-follow-keys-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.log"))
    }

    fn keys(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.log.keys"))
    }

    fn open(&self, name: &str) -> FileStore {
        let storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(&self.keys(name)));
        let keys = Keys::open(storage).expect("opens the key file");
        Store::open_sealed(
            FileStorage::open(&self.log(name)).expect("opens file"),
            keys,
            SystemClock,
        )
        .expect("opens")
    }

    /// Backs `from` up and restores the backup as the store `name`: how a
    /// replica starts.
    fn copy_of(&self, from: &mut FileStore, name: &str) -> FileStore {
        let dir = self.0.join(format!("backup-for-{name}"));
        let mut target = DirTarget::open(&dir).expect("opens the backup");
        from.backup(&mut target).expect("backs up");
        // The place is locked while it is written to, and on Windows
        // nothing reads it until that ends.
        drop(target);
        let mut log = FileStorage::open(&self.log(name)).expect("opens file");
        let mut keys = FileKeyStorage::new(&self.keys(name));
        restore(&mut DirSource::new(&dir), &mut log, Some(&mut keys)).expect("restores");
        drop(log);
        self.open(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

fn commit(store: &mut FileStore, key: &str, value: &str) -> CommitId {
    store
        .commit(MAIN_BRANCH, vec![set(key, value)], by("seed", "test data"))
        .expect("commits")
}

/// A primary with a few commits about two players.
fn primary(dir: &TempDir) -> FileStore {
    let mut store = dir.open("primary");
    commit(&mut store, "player/1/gems", "100");
    commit(&mut store, "player/2/gems", "7");
    store
}

/// Asks until the replica holds what the primary does.
fn catch_up(primary: &mut impl Primary, replica: &mut impl Follower, max_bytes: usize) {
    loop {
        let progress = follow_once(primary, replica, max_bytes).expect("follows");
        if progress.is_caught_up() {
            return;
        }
    }
}

fn assert_same(replica: &FileStore, primary: &FileStore) {
    assert_eq!(replica.head(MAIN_BRANCH), primary.head(MAIN_BRANCH));
    assert_eq!(replica.entries(MAIN_BRANCH), primary.entries(MAIN_BRANCH));
    assert_eq!(replica.erasures(), primary.erasures());
    assert_eq!(replica.log_records(), primary.log_records());
}

/// A link that carries keys, and gives another key than each asked for.
struct WrongKeys<'a>(&'a mut FileStore);

impl Primary for WrongKeys<'_> {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        self.0.ask(ask)
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        Ok(Some(
            wanted
                .iter()
                .map(|_| KeyAnswer::Key([7; KEY_LEN]))
                .collect(),
        ))
    }
}

/// A link that carries keys, and says of each asked for that another
/// was destroyed.
struct ErasedOthers<'a>(&'a mut FileStore);

impl Primary for ErasedOthers<'_> {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        self.0.ask(ask)
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        Ok(Some(
            wanted
                .iter()
                .map(|wanted| KeyAnswer::Erased {
                    key: wanted.map(|byte| !byte),
                    at: 1,
                })
                .collect(),
        ))
    }
}

/// A link that carries keys, and answers for fewer than were asked for.
struct TooFewKeys<'a>(&'a mut FileStore);

impl Primary for TooFewKeys<'_> {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        self.0.ask(ask)
    }

    fn keys(&mut self, _: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        Ok(Some(Vec::new()))
    }
}

#[test]
fn a_link_that_carries_keys_brings_the_key_of_a_subject_the_replica_never_had() {
    let dir = TempDir::new("brings");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    commit(&mut primary, "player/4/gems", "2");
    commit(&mut primary, "player/3/gems", "3");

    catch_up(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);

    assert_same(&replica, &primary);
    assert_eq!(
        replica.get(MAIN_BRANCH, b"player/3/gems").as_deref(),
        Some(&b"3"[..])
    );
    assert_eq!(
        replica.get(MAIN_BRANCH, b"player/4/gems").as_deref(),
        Some(&b"2"[..])
    );
}

#[test]
fn a_key_that_travelled_is_on_the_replicas_disk_before_the_records_are() {
    let dir = TempDir::new("durable");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    catch_up(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);
    drop(replica);

    // Opening checks that every key the log needs is in the key file.
    let reopened = dir.open("replica");

    assert_eq!(
        reopened.get(MAIN_BRANCH, b"player/3/gems").as_deref(),
        Some(&b"1"[..])
    );
}

#[test]
fn a_key_is_asked_for_once() {
    let dir = TempDir::new("once");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    catch_up(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);
    commit(&mut primary, "player/3/gems", "2");

    // No key travels on this link, and none is needed.
    catch_up(&mut primary, &mut replica, MAX_SENT);

    assert_eq!(
        replica.get(MAIN_BRANCH, b"player/3/gems").as_deref(),
        Some(&b"2"[..])
    );
}

#[test]
fn a_key_that_is_not_the_one_the_records_name_is_not_kept() {
    let dir = TempDir::new("wrong");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    let held = replica.log_records();
    let keys_before = std::fs::read(dir.keys("replica")).expect("reads the key file");

    let wrong = follow_once(&mut WrongKeys(&mut primary), &mut replica, MAX_SENT);
    let few = follow_once(&mut TooFewKeys(&mut primary), &mut replica, MAX_SENT);
    let others = follow_once(&mut ErasedOthers(&mut primary), &mut replica, MAX_SENT);

    // What arrived is not what was asked for: worth asking again, as for
    // an answer that did not arrive.
    assert!(matches!(wrong, Err(FollowError::Link(_))), "{wrong:?}");
    assert!(matches!(few, Err(FollowError::Link(_))), "{few:?}");
    // Least of all does it stop for good on the word that some other
    // key was destroyed.
    assert!(matches!(others, Err(FollowError::Link(_))), "{others:?}");
    assert_eq!(replica.log_records(), held);
    assert_eq!(
        std::fs::read(dir.keys("replica")).expect("reads the key file"),
        keys_before
    );
    catch_up(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);
    assert_same(&replica, &primary);
}

#[test]
fn a_key_destroyed_before_the_replica_had_it_brings_the_records_through_its_erasure() {
    let dir = TempDir::new("destroyed");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    commit(&mut primary, "player/1/gems", "150");
    primary
        .erase(b"player/3", by("ops", "asked to be forgotten"))
        .expect("erases");
    commit(&mut primary, "player/1/gems", "175");

    // One record an answer, so the erasure is not in the answer that
    // holds the value: the primary says where it is, and is asked for
    // the records through it.
    catch_up(&mut WithKeys(&mut primary), &mut replica, 1);

    assert_same(&replica, &primary);
    assert_eq!(replica.get(MAIN_BRANCH, b"player/3/gems"), None);
    drop(replica);
    let reopened = dir.open("replica");
    assert_eq!(
        reopened.get(MAIN_BRANCH, b"player/1/gems").as_deref(),
        Some(&b"175"[..])
    );
}

#[test]
fn an_erasure_further_on_than_a_primary_sends_at_once_stops_the_replica() {
    let dir = TempDir::new("far");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    // More between the value and its erasure than a primary sends in
    // one answer, whatever it is asked.
    let large = "g".repeat(MAX_THROUGH / 4);
    for _ in 0..5 {
        commit(&mut primary, "player/1/replay", &large);
    }
    primary
        .erase(b"player/3", by("ops", "asked to be forgotten"))
        .expect("erases");
    let held = replica.log_records();

    let stopped = follow_once(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);

    assert!(
        matches!(stopped, Err(FollowError::Refused(Refusal::TooFar))),
        "{stopped:?}"
    );
    assert_eq!(replica.log_records(), held);
}

#[test]
fn a_subject_written_and_erased_within_one_answer_needs_no_key() {
    let dir = TempDir::new("within");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    primary
        .erase(b"player/3", by("ops", "asked to be forgotten"))
        .expect("erases");

    catch_up(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);

    assert_same(&replica, &primary);
    assert_eq!(replica.get(MAIN_BRANCH, b"player/3/gems"), None);
}

#[test]
fn a_key_the_primary_never_had_stops_the_replica_as_a_missing_key() {
    let dir = TempDir::new("never");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    let ask = Ask {
        held: replica.held().expect("reads its log"),
        max_bytes: MAX_SENT,
        through: None,
    };
    let sent = primary.ask(&ask).expect("answers").expect("sends");
    let wanted = replica.lacks(&sent).expect("reads what it was sent");

    // Asked of a store that never held the key: the replica's own.
    let answers = umbrachyx::store::follow::Feed::keys(&mut replica, &wanted).expect("answers");

    assert_eq!(wanted.len(), 1);
    assert_eq!(answers, [KeyAnswer::Unknown(wanted[0])]);
}

#[test]
fn writers_carry_keys_between_two_committers() {
    let dir = TempDir::new("committers");
    let mut store = primary(&dir);
    let replica = Committer::start_following(dir.copy_of(&mut store, "replica"));
    commit(&mut store, "player/3/gems", "1");
    let primary = Committer::start(store);
    let mut feed = primary.writer();
    let mut follower = replica.writer();
    let reader = replica.reader();

    catch_up(&mut WithKeys(&mut feed), &mut follower, MAX_SENT);

    assert_eq!(
        reader.view().get(MAIN_BRANCH, b"player/3/gems").as_deref(),
        Some(&b"1"[..])
    );
}

#[test]
fn a_subject_erased_and_written_again_gets_its_new_key_on_the_replica() {
    let dir = TempDir::new("again");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    primary
        .erase(b"player/2", by("ops", "asked to be forgotten"))
        .expect("erases");
    // The same subject under a new key, while the replica still holds
    // the old one: the erasure and the new value come in one answer.
    commit(&mut primary, "player/2/gems", "9");

    catch_up(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);

    assert_same(&replica, &primary);
    assert_eq!(
        replica.get(MAIN_BRANCH, b"player/2/gems").as_deref(),
        Some(&b"9"[..])
    );
    drop(replica);
    let reopened = dir.open("replica");
    assert_eq!(
        reopened.get(MAIN_BRANCH, b"player/2/gems").as_deref(),
        Some(&b"9"[..])
    );
}

#[test]
fn a_new_key_behind_an_erasure_and_a_key_destroyed_first_stop_the_replica_as_destroyed() {
    let dir = TempDir::new("both");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    primary
        .erase(b"player/2", by("ops", "asked to be forgotten"))
        .expect("erases");
    commit(&mut primary, "player/2/gems", "9");
    primary
        .erase(b"player/3", by("ops", "asked to be forgotten"))
        .expect("erases");
    let held = replica.log_records();

    // The replica cannot keep `player/2`'s new key while it holds the
    // old, and the records up to that erasure need `player/3`'s key,
    // which is destroyed.
    let stopped = follow_once(&mut WithKeys(&mut primary), &mut replica, MAX_SENT);

    assert!(
        matches!(stopped, Err(FollowError::Store(StoreError::KeysErased(1)))),
        "{stopped:?}"
    );
    assert_eq!(replica.log_records(), held);
}

/// A link that carries keys, to a primary that erases a subject after it
/// answered with records and before it is asked for keys, the first
/// time only.
struct ErasedMeanwhile<'a> {
    store: &'a mut FileStore,
    subject: Option<&'static [u8]>,
}

impl Primary for ErasedMeanwhile<'_> {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        self.store.ask(ask)
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        if let Some(subject) = self.subject.take() {
            self.store
                .erase(subject, by("ops", "asked to be forgotten"))
                .expect("erases");
        }
        WithKeys(&mut *self.store).keys(wanted)
    }
}

#[test]
fn a_subject_erased_between_the_answer_and_the_ask_for_its_key_does_not_stop_the_replica() {
    let dir = TempDir::new("meanwhile");
    let mut primary = primary(&dir);
    let mut replica = dir.copy_of(&mut primary, "replica");
    commit(&mut primary, "player/3/gems", "1");
    let mut link = ErasedMeanwhile {
        store: &mut primary,
        subject: Some(b"player/3"),
    };

    // The key is gone by the time it is asked for, and the erasure is in
    // the log by then: asked again, the primary sends both.
    let progress = follow_once(&mut link, &mut replica, MAX_SENT).expect("follows");

    assert!(progress.is_caught_up(), "{progress:?}");
    assert_same(&replica, &primary);
    assert_eq!(replica.get(MAIN_BRANCH, b"player/3/gems"), None);
}
