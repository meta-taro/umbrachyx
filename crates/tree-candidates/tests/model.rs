//! Every candidate is checked against `BTreeMap` as the model: lookups,
//! persistence of old versions, and diffs.

use std::collections::BTreeMap;

use umbrachyx_tree_candidates::btree::BPlusTree;
use umbrachyx_tree_candidates::data::{self, Rng};
use umbrachyx_tree_candidates::hamt::HamtTree;
use umbrachyx_tree_candidates::prolly::ProllyTree;
use umbrachyx_tree_candidates::{Bytes, Change, Tree};

type Model = BTreeMap<Bytes, Bytes>;

fn build<T: Tree>(model: &Model) -> T {
    T::from_sorted(model.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
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

/// Half updates of existing keys, half new keys between existing ones.
fn random_changes(rng: &mut Rng, records: u64, count: usize) -> Vec<(Bytes, Bytes)> {
    (0..count)
        .map(|i| {
            let key = if i.is_multiple_of(2) {
                data::key(rng.below(records))
            } else {
                data::new_key(rng, records)
            };
            (key, data::value(rng))
        })
        .collect()
}

fn apply<T: Tree>(tree: &T, model: &Model, changes: &[(Bytes, Bytes)]) -> (T, Model) {
    let mut tree = tree.clone();
    let mut model = model.clone();
    for (key, value) in changes {
        tree = tree.insert(key.clone(), value.clone());
        model.insert(key.clone(), value.clone());
    }
    (tree, model)
}

fn lookups_match_the_model<T: Tree>() {
    for seed in 0..20 {
        let mut rng = Rng::new(seed);
        let records = 1 + rng.below(3_000);
        let model: Model = data::dataset(records, &mut rng).into_iter().collect();
        let base: T = build(&model);
        let changes = random_changes(&mut rng, records, 500);

        let (tree, changed) = apply(&base, &model, &changes);

        for (key, value) in &changed {
            assert_eq!(tree.get(key), Some(value), "{} seed {seed}", T::NAME);
        }
        for (key, value) in &model {
            assert_eq!(
                base.get(key),
                Some(value),
                "{} old version, seed {seed}",
                T::NAME
            );
        }
        assert_eq!(tree.get(b"player/x"), None);
    }
}

fn diffs_match_the_model<T: Tree>() {
    for seed in 0..20 {
        let mut rng = Rng::new(seed);
        let records = 1 + rng.below(5_000);
        let model: Model = data::dataset(records, &mut rng).into_iter().collect();
        let base: T = build(&model);
        let count = 1 + rng.below(200) as usize;
        let changes = random_changes(&mut rng, records, count);

        let (derived, changed) = apply(&base, &model, &changes);
        let rebuilt: T = build(&changed);

        let expected = model_diff(&model, &changed);
        assert_eq!(
            base.diff(&derived).changes,
            expected,
            "{} derived, seed {seed}",
            T::NAME
        );
        assert_eq!(
            base.diff(&rebuilt).changes,
            expected,
            "{} rebuilt, seed {seed}",
            T::NAME
        );
        let reverse = model_diff(&changed, &model);
        assert_eq!(
            derived.diff(&base).changes,
            reverse,
            "{} reverse, seed {seed}",
            T::NAME
        );
    }
}

fn identical_trees_diff_without_reading_below_the_root<T: Tree>() {
    let mut rng = Rng::new(3);
    let model: Model = data::dataset(10_000, &mut rng).into_iter().collect();
    let tree: T = build(&model);

    let diff = tree.diff(&tree.clone());

    assert!(diff.changes.is_empty());
    assert!(
        diff.nodes_visited <= 2 * 8,
        "{} read {}",
        T::NAME,
        diff.nodes_visited
    );
}

fn empty_trees_behave<T: Tree>() {
    let empty = T::empty();
    let one = empty.insert(data::key(0), Bytes::from(&b"v"[..]));

    assert_eq!(empty.get(b"anything"), None);
    assert_eq!(T::from_sorted(Vec::new()).root_hash(), empty.root_hash());
    assert_eq!(empty.diff(&one).changes.len(), 1);
    assert_ne!(empty.root_hash(), one.root_hash());
}

/// The shape, and so the root hash, depends only on the content.
fn shape_does_not_depend_on_insert_order<T: Tree>() {
    for seed in 0..10 {
        let mut rng = Rng::new(seed);
        let entries = data::dataset(2_000, &mut rng);
        let mut shuffled = entries.clone();
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.below(i as u64 + 1) as usize);
        }

        let inserted = shuffled
            .into_iter()
            .fold(T::empty(), |tree, (key, value)| tree.insert(key, value));

        assert_eq!(
            inserted.root_hash(),
            T::from_sorted(entries).root_hash(),
            "{} seed {seed}",
            T::NAME
        );
    }
}

/// Growing a tree by inserts gives the same root as building it from the
/// final data, in any insert order, and at a size where the tree is several
/// levels high.
fn grown_equals_built<T: Tree>() {
    let records = 40_000;
    let mut rng = Rng::new(11);
    let model: Model = data::dataset(records, &mut rng).into_iter().collect();
    let changes = random_changes(&mut rng, records, 2_000);
    let base: T = build(&model);
    let (derived, changed) = apply(&base, &model, &changes);
    assert_eq!(
        derived.root_hash(),
        build::<T>(&changed).root_hash(),
        "{} derived",
        T::NAME
    );

    let entries: Vec<(Bytes, Bytes)> = data::dataset(20_000, &mut rng);
    let ascending = entries
        .iter()
        .cloned()
        .fold(T::empty(), |tree, (key, value)| tree.insert(key, value));
    let descending = entries
        .iter()
        .rev()
        .cloned()
        .fold(T::empty(), |tree, (key, value)| tree.insert(key, value));
    let built = T::from_sorted(entries);
    assert_eq!(
        ascending.root_hash(),
        built.root_hash(),
        "{} ascending",
        T::NAME
    );
    assert_eq!(
        descending.root_hash(),
        built.root_hash(),
        "{} descending",
        T::NAME
    );
}

/// Two trees built separately from unrelated key sets, from empty to
/// thousands of keys, including one that is a prefix of the other.
fn separately_built_trees_diff_like_the_model<T: Tree>() {
    for seed in 0..30 {
        let mut rng = Rng::new(seed);
        let pick = |rng: &mut Rng, size: u64| -> Model {
            let mut model = Model::new();
            for _ in 0..size {
                let index = rng.below(8_000);
                model.insert(data::key(index), data::value(rng));
            }
            model
        };
        let old_size = [0, 1, 50, 3_000][seed as usize % 4];
        let new_size = [3_000, 0, 1, 50, 2_000][seed as usize % 5];
        let old = pick(&mut rng, old_size);
        let mut new = pick(&mut rng, new_size);
        if seed % 3 == 0 {
            // A prefix: the new tree keeps the old one's first half.
            new = old
                .iter()
                .take(old.len() / 2)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        }

        let diff = build::<T>(&old).diff(&build::<T>(&new));

        assert_eq!(
            diff.changes,
            model_diff(&old, &new),
            "{} seed {seed}",
            T::NAME
        );
    }
}

fn separately_built_identical_trees_diff_at_the_root<T: Tree>() {
    let mut rng = Rng::new(5);
    let model: Model = data::dataset(10_000, &mut rng).into_iter().collect();

    let diff = build::<T>(&model).diff(&build::<T>(&model));

    assert!(diff.changes.is_empty());
    assert!(
        diff.nodes_visited <= 2,
        "{} read {}",
        T::NAME,
        diff.nodes_visited
    );
}

macro_rules! candidate {
    ($name:ident, $tree:ty) => {
        mod $name {
            use super::*;

            #[test]
            fn lookups_match_the_model() {
                super::lookups_match_the_model::<$tree>();
            }

            #[test]
            fn diffs_match_the_model() {
                super::diffs_match_the_model::<$tree>();
            }

            #[test]
            fn identical_trees_diff_without_reading_below_the_root() {
                super::identical_trees_diff_without_reading_below_the_root::<$tree>();
            }

            #[test]
            fn separately_built_trees_diff_like_the_model() {
                super::separately_built_trees_diff_like_the_model::<$tree>();
            }

            #[test]
            fn empty_trees_behave() {
                super::empty_trees_behave::<$tree>();
            }
        }
    };
}

candidate!(prolly, ProllyTree);
candidate!(bplus, BPlusTree);
candidate!(hamt, HamtTree);

#[test]
fn prolly_grown_equals_built() {
    grown_equals_built::<ProllyTree>();
}

#[test]
fn hamt_grown_equals_built() {
    grown_equals_built::<HamtTree>();
}

#[test]
fn prolly_identical_trees_built_apart_diff_at_the_root() {
    separately_built_identical_trees_diff_at_the_root::<ProllyTree>();
}

#[test]
fn hamt_identical_trees_built_apart_diff_at_the_root() {
    separately_built_identical_trees_diff_at_the_root::<HamtTree>();
}

#[test]
fn prolly_shape_does_not_depend_on_insert_order() {
    shape_does_not_depend_on_insert_order::<ProllyTree>();
}

#[test]
fn hamt_shape_does_not_depend_on_insert_order() {
    shape_does_not_depend_on_insert_order::<HamtTree>();
}

/// The property the other two have and this one lacks, pinned so the
/// benchmark's explanation stays true.
#[test]
fn bplus_shape_depends_on_insert_order() {
    let mut rng = Rng::new(0);
    let entries = data::dataset(2_000, &mut rng);

    let inserted = entries
        .iter()
        .cloned()
        .fold(BPlusTree::empty(), |tree, (key, value)| {
            tree.insert(key, value)
        });

    assert_ne!(
        inserted.root_hash(),
        BPlusTree::from_sorted(entries).root_hash()
    );
}

/// Ten changes in 50,000 records, as a derived version and as a tree built
/// separately: (nodes read by the first diff, by the second).
fn nodes_read_for_ten_changes<T: Tree>() -> (usize, usize) {
    let records = 50_000;
    let mut rng = Rng::new(7);
    let model: Model = data::dataset(records, &mut rng).into_iter().collect();
    let base: T = build(&model);
    let changes = random_changes(&mut rng, records, 10);

    let (derived, changed) = apply(&base, &model, &changes);
    let rebuilt: T = build(&changed);

    (
        base.diff(&derived).nodes_visited,
        base.diff(&rebuilt).nodes_visited,
    )
}

/// About 1,000 leaves hold the records; a diff around ten changes should
/// read a few nodes per change on each side.
const FEW_NODES: usize = 200;

#[test]
fn prolly_diff_reads_around_the_changes_either_way() {
    let (derived, rebuilt) = nodes_read_for_ten_changes::<ProllyTree>();

    assert!(
        derived <= FEW_NODES && rebuilt <= FEW_NODES,
        "{derived}, {rebuilt}"
    );
}

#[test]
fn hamt_diff_reads_around_the_changes_either_way() {
    let (derived, rebuilt) = nodes_read_for_ten_changes::<HamtTree>();

    assert!(
        derived <= FEW_NODES && rebuilt <= FEW_NODES,
        "{derived}, {rebuilt}"
    );
}

#[test]
fn bplus_diff_reads_around_the_changes_only_when_derived() {
    let (derived, rebuilt) = nodes_read_for_ten_changes::<BPlusTree>();

    assert!(derived <= FEW_NODES, "{derived}");
    assert!(rebuilt > 5 * FEW_NODES, "{rebuilt}");
}
