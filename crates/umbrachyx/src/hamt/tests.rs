use std::collections::BTreeMap;

use super::*;

fn bytes(text: &str) -> Bytes {
    Arc::from(text.as_bytes())
}

/// A small generator so the tests need no dependency: SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn model_entries(model: &BTreeMap<Bytes, Bytes>) -> Vec<(Bytes, Bytes)> {
    model
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[test]
fn an_empty_map_holds_nothing() {
    let map = Hamt::empty();

    assert!(map.is_empty());
    assert_eq!(map.len(), 0);
    assert_eq!(map.get(b"anything"), None);
}

#[test]
fn insert_then_get_and_replace() {
    let map = Hamt::empty().insert(bytes("pos/1/x"), bytes("10"));
    let replaced = map.insert(bytes("pos/1/x"), bytes("11"));

    assert_eq!(map.get(b"pos/1/x"), Some(&bytes("10")));
    assert_eq!(replaced.get(b"pos/1/x"), Some(&bytes("11")));
    assert_eq!(replaced.len(), 1);
}

#[test]
fn an_old_version_is_untouched_by_later_changes() {
    let mut versions = vec![Hamt::empty()];
    for i in 0..500 {
        let next = versions[i].insert(bytes(&format!("k{i}")), bytes(&format!("v{i}")));
        versions.push(next);
    }
    let removed = versions[500].remove(b"k7");

    assert_eq!(versions[100].len(), 100);
    assert_eq!(versions[100].get(b"k99"), Some(&bytes("v99")));
    assert_eq!(versions[100].get(b"k100"), None);
    assert_eq!(versions[500].get(b"k7"), Some(&bytes("v7")));
    assert_eq!(removed.get(b"k7"), None);
    assert_eq!(removed.len(), 499);
}

#[test]
fn removing_an_absent_key_changes_nothing() {
    let map = Hamt::empty().insert(bytes("a"), bytes("1"));
    let same = map.remove(b"b");

    assert_eq!(same.len(), 1);
    assert!(same.same_root(&map));
}

#[test]
fn removing_every_key_leaves_an_empty_map() {
    let keys: Vec<String> = (0..2_000).map(|i| format!("pos/{i}/x")).collect();
    let mut map = Hamt::empty();
    for key in &keys {
        map = map.insert(bytes(key), bytes("v"));
    }
    assert!(map.depth() > 0, "2,000 keys need more than one leaf");
    for key in &keys {
        map = map.remove(key.as_bytes());
    }

    assert!(map.is_empty());
    assert_eq!(map.depth(), 0, "an emptied map collapses back to one leaf");
}

#[test]
fn matches_a_sorted_map_through_random_changes() {
    let mut rng = Rng(7);
    let mut map = Hamt::empty();
    let mut model: BTreeMap<Bytes, Bytes> = BTreeMap::new();
    for step in 0..20_000 {
        let key = bytes(&format!("t/{}", rng.below(3_000)));
        if rng.below(4) == 0 {
            map = map.remove(&key);
            model.remove(&key);
        } else {
            let value = bytes(&format!("{step}"));
            map = map.insert(key.clone(), value.clone());
            model.insert(key, value);
        }
    }

    let mut visited: Vec<(Bytes, Bytes)> = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    visited.sort();

    assert_eq!(map.len(), model.len());
    assert_eq!(map.sorted_entries(), model_entries(&model));
    assert_eq!(visited, model_entries(&model));
    for key in model.keys() {
        assert_eq!(map.get(key), model.get(key));
    }
}

fn expected_changes(
    before: &BTreeMap<Bytes, Bytes>,
    after: &BTreeMap<Bytes, Bytes>,
) -> Vec<Change> {
    let mut expected = Vec::new();
    for (key, old) in before {
        match after.get(key) {
            None => expected.push(Change::Removed {
                key: key.clone(),
                value: old.clone(),
            }),
            Some(new) if new != old => expected.push(Change::Modified {
                key: key.clone(),
                old: old.clone(),
                new: new.clone(),
            }),
            Some(_) => {}
        }
    }
    for (key, new) in after {
        if !before.contains_key(key) {
            expected.push(Change::Added {
                key: key.clone(),
                value: new.clone(),
            });
        }
    }
    expected.sort_by(|a, b| a.key().cmp(b.key()));
    expected
}

#[test]
fn the_diff_of_two_versions_is_what_changed() {
    let mut rng = Rng(11);
    let mut base = Hamt::empty();
    for i in 0..5_000 {
        base = base.insert(bytes(&format!("t/{i}")), bytes("0"));
    }
    let mut changed = base.clone();
    let mut model: BTreeMap<Bytes, Bytes> = base.sorted_entries().into_iter().collect();
    for step in 0..300 {
        let key = bytes(&format!("t/{}", rng.below(6_000)));
        if rng.below(3) == 0 {
            changed = changed.remove(&key);
            model.remove(&key);
        } else {
            let value = bytes(&format!("s{step}"));
            changed = changed.insert(key.clone(), value.clone());
            model.insert(key, value);
        }
    }
    let before: BTreeMap<Bytes, Bytes> = base.sorted_entries().into_iter().collect();

    let diff = base.diff(&changed);

    assert_eq!(diff.changes, expected_changes(&before, &model));
}

#[test]
fn a_diff_reads_only_around_the_change_when_versions_share_nodes() {
    let mut base = Hamt::empty();
    for i in 0..50_000 {
        base = base.insert(bytes(&format!("t/{i}")), bytes("0"));
    }
    let changed = base
        .insert(bytes("t/123"), bytes("1"))
        .insert(bytes("t/40000"), bytes("1"));

    let diff = base.diff(&changed);

    assert_eq!(diff.changes.len(), 2);
    assert!(
        diff.nodes_visited < 40,
        "read {} nodes for 2 changes in 50,000 keys",
        diff.nodes_visited
    );
}

#[test]
fn maps_built_in_different_orders_diff_to_nothing() {
    let mut forward = Hamt::empty();
    let mut backward = Hamt::empty();
    for i in 0..3_000 {
        forward = forward.insert(bytes(&format!("t/{i}")), bytes("v"));
        backward = backward.insert(bytes(&format!("t/{}", 2_999 - i)), bytes("v"));
    }

    assert!(forward.diff(&backward).changes.is_empty());
}
