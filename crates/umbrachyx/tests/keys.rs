//! The key file: one key per subject, kept across reopening, and gone for
//! good once the subject is erased.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use umbrachyx::keys::{FileKeyStorage, KEY_LEN, Key, KeyError, KeyStorage, Keys, fingerprint};

/// What a key file starts with, in front of its keys.
const HEADER_LEN: usize = 16;

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-keys-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn keys(&self) -> PathBuf {
        self.0.join("log.keys")
    }

    fn open(&self) -> Result<Keys<FileKeyStorage>, KeyError> {
        Keys::open(FileKeyStorage::new(&self.keys()))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The key file, with its next write failing part way: an append leaves
/// the first half of its bytes, a replacement leaves the old file.
struct FailNextWrite {
    file: FileKeyStorage,
    path: PathBuf,
    armed: bool,
}

impl FailNextWrite {
    fn new(path: &Path) -> Self {
        Self {
            file: FileKeyStorage::new(path),
            path: path.to_owned(),
            armed: true,
        }
    }
}

impl KeyStorage for FailNextWrite {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if !std::mem::take(&mut self.armed) {
            return self.file.append(bytes);
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&bytes[..bytes.len() / 2])?;
        Err(io::Error::other("the disk filled up"))
    }

    fn replace(&mut self, bytes: &[u8]) -> io::Result<()> {
        if !std::mem::take(&mut self.armed) {
            return self.file.replace(bytes);
        }
        Err(io::Error::other("the disk filled up"))
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn a_subject_keeps_its_key_across_reopening() {
    let dir = TempDir::new("reopen");
    let mut keys = dir.open().expect("opens");

    let first = keys.key_for(b"player/42").expect("makes a key");
    let again = keys.key_for(b"player/42").expect("finds the key");
    let other = keys.key_for(b"player/7").expect("makes a key");
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert_eq!(first, again);
    assert_ne!(first, other);
    assert_eq!(reopened.get(b"player/42"), Some(&first));
    assert_eq!(reopened.get(b"player/7"), Some(&other));
    assert_eq!(reopened.get(b"player/8"), None);
}

#[test]
fn an_erased_key_is_gone_from_memory_and_from_the_file() {
    let dir = TempDir::new("erase");
    let mut keys = dir.open().expect("opens");
    let erased = keys.key_for(b"player/42").expect("makes a key");
    let kept = keys.key_for(b"player/7").expect("makes a key");

    let was_there = keys.erase(b"player/42").expect("erases");
    let again = keys.erase(b"player/42").expect("erases nothing");
    drop(keys);
    let reopened = dir.open().expect("reopens");
    let file = std::fs::read(dir.keys()).expect("reads the file");

    assert!(was_there);
    assert!(!again);
    assert_eq!(reopened.get(b"player/42"), None);
    assert_eq!(reopened.get(b"player/7"), Some(&kept));
    assert!(!contains(&file, &erased[..]));
    assert!(!contains(&file, b"player/42"));
}

#[test]
fn a_new_key_after_erasing_is_a_different_key() {
    let dir = TempDir::new("after-erase");
    let mut keys = dir.open().expect("opens");
    let before = keys.key_for(b"player/42").expect("makes a key");
    keys.erase(b"player/42").expect("erases");

    let after = keys.key_for(b"player/42").expect("makes a key");

    assert_ne!(before, after);
}

/// A crash while a key was being appended leaves part of it. That key was
/// never used, since nothing is written under a key before it is synced.
#[test]
fn a_torn_key_at_the_end_is_dropped_and_appending_goes_on() {
    let dir = TempDir::new("torn");
    let mut keys = dir.open().expect("opens");
    let kept = keys.key_for(b"player/42").expect("makes a key");
    drop(keys);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.keys())
        .expect("opens the file");
    file.write_all(&[9, 0, b'p', b'l', b'a'])
        .expect("writes a torn entry");
    drop(file);

    let mut keys = dir.open().expect("opens past the torn entry");
    let added = keys.key_for(b"player/7").expect("makes a key");
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert_eq!(reopened.get(b"player/42"), Some(&kept));
    assert_eq!(reopened.get(b"player/7"), Some(&added));
}

/// Damage anywhere but the end is not a torn append, and keys after it
/// may be in use: opening refuses rather than drop them.
#[test]
fn damage_before_the_last_key_is_an_error() {
    let dir = TempDir::new("damage");
    let mut keys = dir.open().expect("opens");
    keys.key_for(b"player/42").expect("makes a key");
    keys.key_for(b"player/7").expect("makes a key");
    drop(keys);
    let mut bytes = std::fs::read(dir.keys()).expect("reads");
    bytes[HEADER_LEN + 4] ^= 1;
    std::fs::write(dir.keys(), &bytes).expect("writes");

    let opened = dir.open();

    assert!(matches!(opened, Err(KeyError::Damaged)), "{opened:?}");
}

/// The file says what it is before any key, so another file, or a key file
/// of a later version, is not read as damage.
#[test]
fn a_key_file_starts_by_saying_what_it_is() {
    let dir = TempDir::new("header");
    let mut keys = dir.open().expect("opens");
    keys.key_for(b"player/42").expect("makes a key");
    drop(keys);

    let bytes = std::fs::read(dir.keys()).expect("reads");

    assert_eq!(&bytes[..8], b"UMBRAKEY");
    assert_eq!(&bytes[8..12], &1_u32.to_le_bytes());
    assert!(bytes.len() > HEADER_LEN);
}

/// Keys with no header in front of them may be in use, whether the header
/// was damaged or the file is from before key files had one: opening
/// refuses, and the next write must not replace them with a fresh file.
#[test]
fn keys_with_no_header_that_checks_out_are_refused() {
    let dir = TempDir::new("no-header");
    let mut keys = dir.open().expect("opens");
    keys.key_for(b"player/42").expect("makes a key");
    drop(keys);
    let whole = std::fs::read(dir.keys()).expect("reads");

    std::fs::write(dir.keys(), &whole[HEADER_LEN..]).expect("writes");
    let headerless = dir.open();
    assert!(
        matches!(headerless, Err(KeyError::Damaged)),
        "{headerless:?}"
    );

    for bit in 0..HEADER_LEN * 8 {
        let mut bytes = whole.clone();
        bytes[bit / 8] ^= 1 << (bit % 8);
        std::fs::write(dir.keys(), &bytes).expect("writes");

        let opened = dir.open();

        assert!(
            matches!(
                opened,
                Err(KeyError::Damaged | KeyError::NewerFormat { .. })
            ),
            "bit {bit}: {opened:?}"
        );
    }
}

#[test]
fn a_key_file_of_a_later_format_is_refused_as_such() {
    let dir = TempDir::new("newer");
    let mut bytes = b"UMBRAKEY".to_vec();
    bytes.extend(2_u32.to_le_bytes());
    bytes.extend([0; 4]);
    std::fs::write(dir.keys(), &bytes).expect("writes");

    let opened = dir.open();

    assert!(
        matches!(opened, Err(KeyError::NewerFormat { version: 2 })),
        "{opened:?}"
    );
}

/// A crash during the very first write can leave part of the header and
/// nothing else. No key was in use yet, so the file is started again.
#[test]
fn a_header_cut_short_with_no_key_after_it_is_written_again() {
    let dir = TempDir::new("torn-header");
    let mut keys = dir.open().expect("opens");
    keys.key_for(b"player/42").expect("makes a key");
    drop(keys);
    let whole = std::fs::read(dir.keys()).expect("reads");

    for cut in 0..HEADER_LEN {
        std::fs::write(dir.keys(), &whole[..cut]).expect("writes");

        let mut keys = dir.open().expect("opens a file with no key in it");
        let made = keys.key_for(b"player/7").expect("makes a key");
        drop(keys);

        let reopened = dir.open().expect("reopens");
        assert_eq!(reopened.get(b"player/7"), Some(&made), "cut at {cut}");
        assert_eq!(reopened.get(b"player/42"), None, "cut at {cut}");
    }
}

/// Opening only reads. A torn end is cut when the next key is written, so
/// a store that finds a key missing can refuse before anything is lost.
#[test]
fn opening_never_changes_the_file() {
    let dir = TempDir::new("read-only-open");
    let mut keys = dir.open().expect("opens");
    keys.key_for(b"player/42").expect("makes a key");
    drop(keys);
    let mut bytes = std::fs::read(dir.keys()).expect("reads");
    bytes.extend([9, 0, b'p']);
    std::fs::write(dir.keys(), &bytes).expect("writes");

    let keys = dir.open().expect("opens");

    assert!(keys.get(b"player/42").is_some());
    assert_eq!(std::fs::read(dir.keys()).expect("reads"), bytes);
}

/// A torn append can come out as garbage, length included. With no whole
/// key after it, it is a torn end: cut by the next write, not refused.
#[test]
fn a_torn_append_of_garbage_is_a_torn_end() {
    for (name, garbage) in [
        ("long", vec![0xFF, 0xFF, 0x13, 0x37]),
        ("short", [&[5, 0][..], &[0xA5; 100][..]].concat()),
    ] {
        let dir = TempDir::new(&format!("garbage-{name}"));
        let mut keys = dir.open().expect("opens");
        let kept = keys.key_for(b"player/42").expect("makes a key");
        drop(keys);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.keys())
            .expect("opens the file");
        file.write_all(&garbage).expect("writes garbage");
        drop(file);

        let mut keys = dir.open().expect("opens past the garbage");
        let added = keys.key_for(b"player/7").expect("makes a key");
        drop(keys);
        let reopened = dir.open().expect("reopens");

        assert_eq!(reopened.get(b"player/42"), Some(&kept), "{name}");
        assert_eq!(reopened.get(b"player/7"), Some(&added), "{name}");
    }
}

/// A length longer than any subject can be is damage, not a torn append,
/// so a flipped bit in a length cannot make every key after it look torn.
#[test]
fn an_impossible_subject_length_is_damage() {
    let dir = TempDir::new("length");
    let mut keys = dir.open().expect("opens");
    keys.key_for(b"player/42").expect("makes a key");
    keys.key_for(b"player/7").expect("makes a key");
    drop(keys);
    let mut bytes = std::fs::read(dir.keys()).expect("reads");
    bytes[1] = 0x80;
    std::fs::write(dir.keys(), &bytes).expect("writes");

    let opened = dir.open();

    assert!(matches!(opened, Err(KeyError::Damaged)), "{opened:?}");
}

#[test]
fn a_key_is_found_by_its_fingerprint() {
    let dir = TempDir::new("fingerprint");
    let mut keys = dir.open().expect("opens");
    let key = keys.key_for(b"player/42").expect("makes a key");
    let other = keys.key_for(b"player/7").expect("makes a key");

    let fingerprint = umbrachyx::keys::fingerprint(&key);

    assert_ne!(fingerprint, umbrachyx::keys::fingerprint(&other));
    assert_eq!(keys.by_fingerprint(&fingerprint), Some(&key));
    keys.erase(b"player/42").expect("erases");
    assert_eq!(keys.by_fingerprint(&fingerprint), None);
}

#[test]
fn a_subject_longer_than_the_limit_gets_no_key() {
    let dir = TempDir::new("long-subject");
    let mut keys = dir.open().expect("opens");

    let made = keys.key_for(&vec![b'p'; umbrachyx::keys::MAX_SUBJECT_LEN + 1]);

    assert!(matches!(made, Err(KeyError::SubjectTooLong)), "{made:?}");
}

/// The torn bytes a failed append leaves are cut before the next key is
/// written, not buried under it where they would read as damage.
#[test]
fn a_failed_append_does_not_strand_its_torn_bytes() {
    let dir = TempDir::new("failed-append");
    let kept = dir
        .open()
        .expect("opens")
        .key_for(b"player/42")
        .expect("makes a key");
    let mut keys = Keys::open(FailNextWrite::new(&dir.keys())).expect("opens");

    let failed = keys.key_for(b"player/7");
    let added = keys.key_for(b"player/8").expect("makes a key");
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert!(failed.is_err());
    assert_eq!(reopened.get(b"player/42"), Some(&kept));
    assert_eq!(reopened.get(b"player/8"), Some(&added));
}

/// A key whose erasure was asked for is not handed out again, even when
/// the file could not be rewritten: the caller recorded the erasure first
/// and will finish it, and anything sealed under the key would go with it.
#[test]
fn a_key_whose_erasure_failed_is_not_used_again() {
    let dir = TempDir::new("failed-erase");
    let old = dir
        .open()
        .expect("opens")
        .key_for(b"player/42")
        .expect("makes a key");
    let mut keys = Keys::open(FailNextWrite::new(&dir.keys())).expect("opens");

    let failed = keys.erase(b"player/42");
    let missing = keys.get(b"player/42").copied();
    let new = keys.key_for(b"player/42").expect("makes a key");
    drop(keys);
    let reopened = dir.open().expect("reopens");
    let bytes = std::fs::read(dir.keys()).expect("reads the key file");

    assert!(failed.is_err());
    assert_eq!(missing, None);
    assert_ne!(new, old);
    assert_eq!(reopened.get(b"player/42"), Some(&new));
    assert!(!contains(&bytes, &old));
}

/// Counts the writes that make keys durable.
struct CountWrites {
    file: FileKeyStorage,
    writes: usize,
}

impl KeyStorage for CountWrites {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        self.file.read_all()
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes += 1;
        self.file.append(bytes)
    }

    fn replace(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes += 1;
        self.file.replace(bytes)
    }
}

/// Keys for many new subjects at once are made durable with one write, not
/// one each, and are the keys each subject has from then on.
#[test]
fn keys_for_many_new_subjects_are_made_with_one_write() {
    let dir = TempDir::new("batch");
    let storage = CountWrites {
        file: FileKeyStorage::new(&dir.keys()),
        writes: 0,
    };
    let mut keys = Keys::open(storage).expect("opens");
    let had = keys.key_for(b"player/1").expect("makes a key");
    let subjects: Vec<Vec<u8>> = (1..=50)
        .map(|n| format!("player/{n}").into_bytes())
        .collect();

    let writes_before = keys.storage().writes;
    keys.make_keys(subjects.iter().map(Vec::as_slice))
        .expect("makes the keys");
    let writes = keys.storage().writes - writes_before;
    let made: Vec<_> = subjects
        .iter()
        .map(|subject| *keys.get(subject).expect("has a key"))
        .collect();
    keys.make_keys(subjects.iter().map(Vec::as_slice))
        .expect("nothing to make");
    let writes_again = keys.storage().writes - writes_before;
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert_eq!(writes, 1);
    assert_eq!(writes_again, 1, "subjects with keys need no write");
    assert_eq!(made[0], had, "a subject keeps the key it had");
    for (subject, key) in subjects.iter().zip(&made) {
        assert_eq!(reopened.get(subject), Some(key));
    }
}

/// A write of several keys that fails leaves none of them in use: values
/// sealed under a key that is not durable could never be opened again.
#[test]
fn a_failed_write_of_many_keys_leaves_none_of_them() {
    let dir = TempDir::new("batch-failed");
    let mut keys = Keys::open(FailNextWrite::new(&dir.keys())).expect("opens");
    let subjects: [&[u8]; 3] = [b"player/1", b"player/2", b"player/3"];

    let failed = keys.make_keys(subjects.into_iter());
    let none_kept = subjects.iter().all(|subject| keys.get(subject).is_none());
    let later = keys.key_for(b"player/2").expect("makes a key");
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert!(failed.is_err());
    assert!(none_kept);
    assert_eq!(reopened.get(b"player/2"), Some(&later));
    assert_eq!(reopened.get(b"player/1"), None);
    assert_eq!(reopened.get(b"player/3"), None);
}

/// Keys made elsewhere, as a replica is sent them, are kept with one
/// write and found by subject and by fingerprint from then on. A subject
/// that has a key keeps the one it has.
#[test]
fn keys_made_elsewhere_are_kept_with_one_write_and_replace_none() {
    let dir = TempDir::new("put");
    let storage = CountWrites {
        file: FileKeyStorage::new(&dir.keys()),
        writes: 0,
    };
    let mut keys = Keys::open(storage).expect("opens");
    let had = keys.key_for(b"player/1").expect("makes a key");
    let writes_before = keys.storage().writes;
    let given: [(&[u8], Key); 3] = [
        (b"player/1", [1; KEY_LEN]),
        (b"player/2", [2; KEY_LEN]),
        (b"player/3", [3; KEY_LEN]),
    ];

    keys.put_keys(&given).expect("keeps the keys");
    keys.put_keys(&given).expect("nothing to keep");
    let writes = keys.storage().writes - writes_before;
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert_eq!(writes, 1);
    assert_eq!(reopened.get(b"player/1"), Some(&had));
    assert_eq!(reopened.get(b"player/2"), Some(&[2; KEY_LEN]));
    assert_eq!(reopened.get(b"player/3"), Some(&[3; KEY_LEN]));
    assert_eq!(
        reopened.subject_of(&fingerprint(&[3; KEY_LEN])),
        Some(&b"player/3"[..])
    );
    assert_eq!(reopened.subject_of(&fingerprint(&[1; KEY_LEN])), None);
}

/// As with keys made here: a write that fails leaves none of them in use.
#[test]
fn a_failed_write_of_keys_made_elsewhere_leaves_none_of_them() {
    let dir = TempDir::new("put-fails");
    let mut keys = Keys::open(FailNextWrite::new(&dir.keys())).expect("opens");
    let given: [(&[u8], Key); 2] = [(b"player/1", [1; KEY_LEN]), (b"player/2", [2; KEY_LEN])];

    let failed = keys.put_keys(&given);
    let none_kept = keys.get(b"player/1").is_none() && keys.get(b"player/2").is_none();
    keys.put_keys(&given[1..]).expect("keeps the key");
    drop(keys);
    let reopened = dir.open().expect("reopens");

    assert!(failed.is_err());
    assert!(none_kept);
    assert_eq!(reopened.get(b"player/2"), Some(&[2; KEY_LEN]));
    assert_eq!(reopened.get(b"player/1"), None);
}

/// Keys made to be held are in use at once, for values that are not on
/// disk yet either, and reach the file with one write when asked.
#[test]
fn keys_held_are_in_use_at_once_and_in_the_file_once_written() {
    let dir = TempDir::new("held");
    let mut keys = dir.open().expect("opens");

    keys.make_keys_held([&b"session/1"[..], b"session/2"].into_iter())
        .expect("makes the keys");

    let first = *keys.get(b"session/1").expect("in use");
    assert!(keys.has_held());
    let on_disk = dir.open().expect("opens what is on disk");
    assert_eq!(on_disk.get(b"session/1"), None, "not in the file yet");

    keys.write_held().expect("writes them");

    assert!(!keys.has_held());
    let reopened = dir.open().expect("reopens");
    assert_eq!(reopened.get(b"session/1"), Some(&first));
    assert!(reopened.get(b"session/2").is_some());
}

/// A process that ends with keys held loses them, and the file it
/// leaves opens with the keys it had.
#[test]
fn keys_held_when_the_process_ends_are_gone_and_the_file_opens() {
    let dir = TempDir::new("held-lost");
    let mut keys = dir.open().expect("opens");
    let kept = keys.key_for(b"player/1").expect("makes a durable key");
    keys.make_keys_held(std::iter::once(&b"session/1"[..]))
        .expect("makes a held key");
    drop(keys);

    let reopened = dir.open().expect("reopens");

    assert_eq!(reopened.get(b"player/1"), Some(&kept));
    assert_eq!(reopened.get(b"session/1"), None);
}

/// A key made durable takes the held ones into the file with it, ahead
/// of it: none is left behind a later one.
#[test]
fn a_key_made_durable_takes_the_held_ones_with_it() {
    let dir = TempDir::new("held-then-durable");
    let mut keys = dir.open().expect("opens");
    keys.make_keys_held(std::iter::once(&b"session/1"[..]))
        .expect("makes a held key");
    let held = *keys.get(b"session/1").expect("in use");

    let durable = keys.key_for(b"player/1").expect("makes a durable key");

    assert!(!keys.has_held());
    let reopened = dir.open().expect("reopens");
    assert_eq!(reopened.get(b"session/1"), Some(&held));
    assert_eq!(reopened.get(b"player/1"), Some(&durable));
}

/// A write of held keys that fails leaves them in use and held, and the
/// next write puts them all in the file.
#[test]
fn a_failed_write_of_held_keys_keeps_them_for_the_next_write() {
    let dir = TempDir::new("held-fails");
    let mut keys = Keys::open(FailNextWrite::new(&dir.keys())).expect("opens");
    keys.make_keys_held([&b"session/1"[..], b"session/2"].into_iter())
        .expect("makes the keys");
    let first = *keys.get(b"session/1").expect("in use");

    assert!(keys.write_held().is_err(), "the write was made to fail");

    assert!(keys.has_held());
    assert_eq!(keys.get(b"session/1"), Some(&first));
    keys.write_held().expect("the next write goes through");
    assert!(!keys.has_held());
    let reopened = dir.open().expect("reopens");
    assert_eq!(reopened.get(b"session/1"), Some(&first));
    assert!(reopened.get(b"session/2").is_some());
}

/// Erasing a subject whose key is still held writes the file without
/// it, and with every other key in use.
#[test]
fn erasing_a_subject_whose_key_is_held_leaves_the_file_without_it() {
    let dir = TempDir::new("held-erased");
    let mut keys = dir.open().expect("opens");
    keys.make_keys_held([&b"session/1"[..], b"session/2"].into_iter())
        .expect("makes the keys");
    let erased = *keys.get(b"session/1").expect("in use");
    let other = *keys.get(b"session/2").expect("in use");

    assert!(keys.erase(b"session/1").expect("erases"));

    assert!(!keys.has_held(), "the file written holds every key in use");
    let bytes = std::fs::read(dir.keys()).expect("reads the file");
    assert!(!contains(&bytes, &erased));
    let reopened = dir.open().expect("reopens");
    assert_eq!(reopened.get(b"session/1"), None);
    assert_eq!(reopened.get(b"session/2"), Some(&other));
}

/// What may be copied elsewhere leaves out the keys the file does not
/// hold yet, and has them once it does.
#[test]
fn keys_held_are_not_among_those_copied_elsewhere() {
    let dir = TempDir::new("held-not-copied");
    let mut keys = dir.open().expect("opens");
    let durable = keys.key_for(b"player/1").expect("makes a durable key");
    keys.make_keys_held(std::iter::once(&b"session/1"[..]))
        .expect("makes a held key");
    let held = *keys.get(b"session/1").expect("in use");

    let copied = keys.durable_entries().expect("lists the keys");

    assert!(contains(&copied, &durable));
    assert!(!contains(&copied, &held));
    keys.write_held().expect("writes it");
    let copied = keys.durable_entries().expect("lists the keys");
    assert!(contains(&copied, &held));
}
