//! The store on a real file: what was committed comes back after closing and
//! opening again, with the same root hash.

use std::path::PathBuf;

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{Provenance, Store, SystemClock};

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("umbrachyx-store-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creates temp dir");
    dir.join("log")
}

fn provenance(why: &str) -> Provenance {
    Provenance {
        who: "agent:test".to_owned(),
        why: why.to_owned(),
        approved_by: Some("ops:reviewer".to_owned()),
    }
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

fn remove(key: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: None,
    }
}

#[test]
fn reopening_gives_back_the_same_tree() {
    let path = temp_path("reopen");
    let (root, time) = {
        let mut store = Store::open(FileStorage::open(&path).expect("opens"), SystemClock)
            .expect("opens store");
        store
            .commit(
                vec![set("player/1/gold", "100"), set("player/1/sword", "1")],
                provenance("grant starter kit"),
            )
            .expect("commits");
        store
            .commit(
                vec![set("player/1/gold", "80"), remove("player/1/sword")],
                provenance("sell sword"),
            )
            .expect("commits");
        (store.main().root_hash(), store.last_time())
    };

    let store =
        Store::open(FileStorage::open(&path).expect("opens"), SystemClock).expect("reopens store");

    assert_eq!(store.main().root_hash(), root);
    assert_eq!(store.last_time(), time);
    assert_eq!(
        store.get(b"player/1/gold").map(|v| &v[..]),
        Some(&b"80"[..])
    );
    assert_eq!(store.get(b"player/1/sword"), None);
    std::fs::remove_dir_all(path.parent().expect("has parent")).expect("cleans up");
}

#[test]
fn an_earlier_version_of_the_tree_does_not_change() {
    let path = temp_path("version");
    let mut store =
        Store::open(FileStorage::open(&path).expect("opens"), SystemClock).expect("opens store");
    store
        .commit(vec![set("k", "before")], provenance("first"))
        .expect("commits");
    let before = store.main().clone();

    store
        .commit(vec![set("k", "after")], provenance("second"))
        .expect("commits");

    assert_eq!(before.get(b"k").map(|v| &v[..]), Some(&b"before"[..]));
    assert_eq!(store.get(b"k").map(|v| &v[..]), Some(&b"after"[..]));
    drop(store);
    std::fs::remove_dir_all(path.parent().expect("has parent")).expect("cleans up");
}
