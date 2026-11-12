//! Diff of two trees: walk both in key order and skip what they share.

use std::cmp::Ordering;

use super::node::Node;
use super::{Bytes, Change, Diff, Digest};

/// Walks both trees in key order, skipping any subtree that starts at the
/// same place on both sides with the same hash. Equal subtrees hold equal
/// entries, and a parent holds its children's hashes, so a shared child is
/// skipped without being read: only the regions around changes are.
pub(super) fn diff(old: &Node, new: &Node) -> Diff {
    let mut old = Cursor::new(old);
    let mut new = Cursor::new(new);
    let mut changes = Vec::new();
    loop {
        if skip_shared(&mut old, &mut new) {
            continue;
        }
        if old.descend() | new.descend() {
            continue;
        }
        let order = match (old.entry(), new.entry()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some((a, _)), Some((b, _))) => a.cmp(b),
        };
        match order {
            Ordering::Less => {
                let (key, value) = old.entry().expect("checked above");
                changes.push(Change::Removed {
                    key: key.clone(),
                    value: value.into(),
                });
                old.next();
            }
            Ordering::Greater => {
                let (key, value) = new.entry().expect("checked above");
                changes.push(Change::Added {
                    key: key.clone(),
                    value: value.into(),
                });
                new.next();
            }
            Ordering::Equal => {
                let (key, old_value) = old.entry().expect("checked above");
                let (_, new_value) = new.entry().expect("checked above");
                if old_value != new_value {
                    changes.push(Change::Modified {
                        key: key.clone(),
                        old: old_value.into(),
                        new: new_value.into(),
                    });
                }
                old.next();
                new.next();
            }
        }
    }
    Diff {
        changes,
        nodes_visited: old.visited + new.visited,
    }
}

fn skip_shared(old: &mut Cursor<'_>, new: &mut Cursor<'_>) -> bool {
    // Largest subtrees first, so a shared node is skipped whole rather than
    // child by child.
    for old_depth in old.starting_depths() {
        for new_depth in new.starting_depths() {
            if old.hash_at(old_depth) == new.hash_at(new_depth) {
                old.skip(old_depth);
                new.skip(new_depth);
                return true;
            }
        }
    }
    false
}

/// A position in key order: the path from the root to the current item.
/// Each frame holds a node that has been read and the index of the current
/// child or, in a leaf, entry. The current child of an internal node is not
/// read until the diff has to look inside it.
struct Cursor<'a> {
    stack: Vec<(&'a Node, usize)>,
    visited: usize,
}

impl<'a> Cursor<'a> {
    fn new(root: &'a Node) -> Self {
        let mut cursor = Self {
            stack: vec![(root, 0)],
            visited: 1,
        };
        cursor.settle();
        cursor
    }

    /// Reads the current child, if the cursor is on one. Returns whether it
    /// moved.
    fn descend(&mut self) -> bool {
        let Some(&(Node::Internal { children, .. }, index)) = self.stack.last() else {
            return false;
        };
        self.stack.push((&children[index].node, 0));
        self.visited += 1;
        true
    }

    /// Climbs out of finished nodes, so the cursor rests on an item or the
    /// stack is empty at the end.
    fn settle(&mut self) {
        while let Some(&(node, index)) = self.stack.last() {
            if index < node.len() {
                return;
            }
            self.stack.pop();
            if let Some(parent) = self.stack.last_mut() {
                parent.1 += 1;
            }
        }
    }

    fn entry(&self) -> Option<(&'a Bytes, &'a [u8])> {
        match self.stack.last()? {
            (Node::Leaf { leaf, .. }, index) => leaf.get(*index),
            (Node::Internal { .. }, _) => None,
        }
    }

    /// Moves past the current item.
    fn next(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            top.1 += 1;
        }
        self.settle();
    }

    /// The subtrees that start at the current position, shallowest
    /// (largest) first. A depth below the stack's length is a node that has
    /// been read; the stack's length itself is the unread current child.
    fn starting_depths(&self) -> std::ops::Range<usize> {
        let mut depth = self.stack.len();
        while depth > 0 && self.stack[depth - 1].1 == 0 {
            depth -= 1;
        }
        let on_child = matches!(self.stack.last(), Some((Node::Internal { .. }, _)));
        depth..self.stack.len() + usize::from(on_child)
    }

    fn hash_at(&self, depth: usize) -> &'a Digest {
        match self.stack.get(depth) {
            Some((node, _)) => node.hash(),
            None => match self.stack.last() {
                Some(&(Node::Internal { children, .. }, index)) => children[index].node.hash(),
                _ => unreachable!("only an internal node has a current child"),
            },
        }
    }

    /// Moves past the whole subtree at `depth`.
    fn skip(&mut self, depth: usize) {
        if depth < self.stack.len() {
            self.stack.truncate(depth + 1);
            let top = &mut self.stack[depth];
            top.1 = top.0.len();
            self.settle();
        } else {
            self.next();
        }
    }
}
