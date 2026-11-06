//! The tree against `BTreeMap` as the model, with inserts and removes mixed:
//! lookups, old versions, diffs, and a shape that depends only on content.

use std::collections::BTreeMap;
use std::sync::Arc;

use umbrachyx::tree::{Bytes, Change, Tree};

type Model = BTreeMap<Bytes, Bytes>;

/// SplitMix64, so every case is fixed by its seed.
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

fn key(index: u64) -> Bytes {
    Arc::from(format!("player/{:06}/slot/{:02}", index / 50, index % 50).into_bytes())
}

fn value(rng: &mut Rng) -> Bytes {
    Arc::from(rng.next().to_le_bytes().to_vec())
}

fn build(model: &Model) -> Tree {
    Tree::from_sorted(model.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

fn model_diff(old: &Model, new: &Model) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, value) in old {
        match new.get(key) {
            None => changes.push(Change::Removed {
                key: key.clone(),
                value: value.clone(),
            }),
            Some(new_value) if new_value != value => changes.push(Change::Modified {
                key: key.clone(),
                old: value.clone(),
                new: new_value.clone(),
            }),
            Some(_) => {}
        }
    }
    for (key, value) in new {
        if !old.contains_key(key) {
            changes.push(Change::Added {
                key: key.clone(),
                value: value.clone(),
            });
        }
    }
    changes.sort_by(|a, b| a.key().cmp(b.key()));
    changes
}

enum Op {
    Insert(Bytes, Bytes),
    Remove(Bytes),
}

/// About a third removes, including keys that are not there.
fn random_ops(rng: &mut Rng, key_space: u64, count: usize) -> Vec<Op> {
    (0..count)
        .map(|_| {
            let key = key(rng.below(key_space));
            if rng.below(3) == 0 {
                Op::Remove(key)
            } else {
                Op::Insert(key, value(rng))
            }
        })
        .collect()
}

fn apply(tree: &Tree, model: &Model, ops: &[Op]) -> (Tree, Model) {
    let mut tree = tree.clone();
    let mut model = model.clone();
    for op in ops {
        match op {
            Op::Insert(key, value) => {
                tree = tree.insert(key.clone(), value.clone());
                model.insert(key.clone(), value.clone());
            }
            Op::Remove(key) => {
                tree = tree.remove(key);
                model.remove(key);
            }
        }
    }
    (tree, model)
}

fn starting_model(rng: &mut Rng, key_space: u64) -> Model {
    let mut model = Model::new();
    for index in 0..key_space {
        if rng.below(2) == 0 {
            model.insert(key(index), value(rng));
        }
    }
    model
}

#[test]
fn lookups_and_old_versions_match_the_model() {
    for seed in 0..30 {
        let mut rng = Rng(seed);
        let key_space = 1 + rng.below(4_000);
        let model = starting_model(&mut rng, key_space);
        let base = build(&model);
        let ops = random_ops(&mut rng, key_space, 1_500);

        let (tree, changed) = apply(&base, &model, &ops);

        for index in 0..key_space {
            let key = key(index);
            assert_eq!(tree.get(&key), changed.get(&key), "seed {seed}");
            assert_eq!(base.get(&key), model.get(&key), "old version, seed {seed}");
        }
        assert_eq!(tree.len(), changed.len(), "seed {seed}");
    }
}

#[test]
fn diffs_match_the_model() {
    for seed in 0..30 {
        let mut rng = Rng(seed);
        let key_space = 1 + rng.below(4_000);
        let model = starting_model(&mut rng, key_space);
        let base = build(&model);
        let count = 1 + rng.below(300) as usize;
        let ops = random_ops(&mut rng, key_space, count);

        let (derived, changed) = apply(&base, &model, &ops);
        let rebuilt = build(&changed);

        let expected = model_diff(&model, &changed);
        assert_eq!(
            base.diff(&derived).changes,
            expected,
            "derived, seed {seed}"
        );
        assert_eq!(
            base.diff(&rebuilt).changes,
            expected,
            "rebuilt, seed {seed}"
        );
        assert_eq!(
            derived.diff(&base).changes,
            model_diff(&changed, &model),
            "reverse, seed {seed}"
        );
    }
}

/// The shape, and so the root hash, depends only on the content, however
/// the tree got there. Large enough for the tree to be several levels high.
#[test]
fn shape_depends_only_on_content() {
    for seed in 0..10 {
        let mut rng = Rng(seed);
        let key_space = 30_000;
        let model = starting_model(&mut rng, key_space);
        let ops = random_ops(&mut rng, key_space, 6_000);

        let (grown, changed) = apply(&build(&model), &model, &ops);

        assert_eq!(
            grown.root_hash(),
            build(&changed).root_hash(),
            "seed {seed}"
        );
    }
}

#[test]
fn removing_everything_gives_the_empty_tree() {
    let mut rng = Rng(4);
    let model = starting_model(&mut rng, 20_000);
    let mut keys: Vec<Bytes> = model.keys().cloned().collect();
    for i in (1..keys.len()).rev() {
        keys.swap(i, rng.below(i as u64 + 1) as usize);
    }

    let emptied = keys
        .iter()
        .fold(build(&model), |tree, key| tree.remove(key));

    assert_eq!(emptied.root_hash(), Tree::empty().root_hash());
    assert!(emptied.is_empty());
}

/// Removing every key that ends a node forces a merge at each of them, on
/// every level.
#[test]
fn removing_node_boundaries_merges_nodes() {
    let mut rng = Rng(9);
    let model = starting_model(&mut rng, 20_000);
    let tree = build(&model);
    let boundaries = tree.boundary_keys();
    assert!(boundaries.len() > 100, "{}", boundaries.len());

    let mut expected = model.clone();
    let mut pruned = tree.clone();
    for key in &boundaries {
        pruned = pruned.remove(key);
        expected.remove(key);
    }

    assert_eq!(pruned.root_hash(), build(&expected).root_hash());
}

#[test]
fn removing_a_missing_key_changes_nothing() {
    let mut rng = Rng(2);
    let model = starting_model(&mut rng, 1_000);
    let tree = build(&model);

    let same = tree.remove(b"player/999999/slot/99");

    assert_eq!(same.root_hash(), tree.root_hash());
}

#[test]
fn a_diff_of_few_changes_reads_few_nodes() {
    let mut rng = Rng(7);
    let model = starting_model(&mut rng, 100_000);
    let base = build(&model);
    let ops = random_ops(&mut rng, 100_000, 10);

    let (derived, changed) = apply(&base, &model, &ops);
    let rebuilt = build(&changed);

    assert!(base.diff(&derived).nodes_visited <= 200);
    assert!(base.diff(&rebuilt).nodes_visited <= 200);
    assert_eq!(base.diff(&rebuilt).changes, model_diff(&model, &changed));
}

#[test]
fn iterating_gives_every_entry_in_key_order() {
    let mut rng = Rng(12);
    let model = starting_model(&mut rng, 20_000);
    let ops = random_ops(&mut rng, 20_000, 3_000);
    let (tree, changed) = apply(&build(&model), &model, &ops);

    let entries: Vec<(Bytes, Bytes)> = tree.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    assert_eq!(entries, changed.into_iter().collect::<Vec<_>>());
    assert_eq!(Tree::empty().iter().count(), 0);
}

/// A sorted run of inserts, as one commit applies them.
fn random_batch(rng: &mut Rng, key_space: u64, count: usize) -> Model {
    (0..count)
        .map(|_| (key(rng.below(key_space)), value(rng)))
        .collect()
}

/// Inserting a batch at once gives the same tree as inserting its entries
/// one by one: the same entries, length and root hash. Batches range from
/// one key to more keys than the tree holds, and reach past its last key.
#[test]
fn inserting_a_batch_matches_inserting_one_by_one() {
    for seed in 0..40 {
        let mut rng = Rng(seed);
        let key_space = 1 + rng.below(30_000);
        let model = starting_model(&mut rng, key_space);
        let base = build(&model);
        let count = 1 + rng.below(key_space.min(5_000)) as usize;
        let batch = random_batch(&mut rng, key_space + 100, count);

        let batched =
            base.insert_sorted(batch.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

        let mut expected = model.clone();
        expected.extend(batch.clone());
        assert_eq!(batched.len(), expected.len(), "seed {seed}");
        assert_eq!(
            batched.root_hash(),
            build(&expected).root_hash(),
            "seed {seed}"
        );
        assert_eq!(
            base.diff(&batched).changes,
            model_diff(&model, &expected),
            "seed {seed}"
        );
    }
}

#[test]
fn inserting_a_batch_into_the_empty_tree_builds_it() {
    let mut rng = Rng(5);
    let model = starting_model(&mut rng, 20_000);

    let tree =
        Tree::empty().insert_sorted(model.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

    assert_eq!(tree.root_hash(), build(&model).root_hash());
    assert_eq!(tree.len(), model.len());
}

#[test]
fn inserting_an_empty_batch_changes_nothing() {
    let mut rng = Rng(6);
    let tree = build(&starting_model(&mut rng, 1_000));

    let same = tree.insert_sorted(Vec::new());

    assert_eq!(same.root_hash(), tree.root_hash());
    assert_eq!(same.len(), tree.len());
}

/// Walking from after a key gives what a full walk gives past it, wherever
/// the key falls: before every key, on one, between two, past the last.
#[test]
fn iter_after_matches_the_model_from_any_key() {
    for seed in 0..30 {
        let mut rng = Rng(seed);
        let key_space = 1 + rng.below(4_000);
        let model = starting_model(&mut rng, key_space);
        let tree = build(&model);
        let mut starts: Vec<Vec<u8>> = vec![Vec::new(), b"zzz".to_vec()];
        for _ in 0..40 {
            let on = key(rng.below(key_space)).to_vec();
            let mut between = on.clone();
            between.push(0);
            starts.extend([on, between]);
        }

        for after in &starts {
            let walked: Vec<_> = tree.iter_after(after).map(|(k, _)| k.clone()).collect();
            let expected: Vec<_> = model
                .keys()
                .filter(|k| k.as_ref() > after.as_slice())
                .cloned()
                .collect();
            assert_eq!(walked, expected, "seed {seed}, after {after:?}");
        }
    }
}
