//! Which engine holds each table, and which tables have been written.
//!
//! A table is the first part of a key: `player/42/gems` is in `player`. A
//! key of one part, like `config`, is in no table and keeps its history
//! whatever is declared. A table never declared keeps its history too.
//!
//! A table can be declared only before anything is written to it, on any
//! branch. Changing how a table that has keys is held would mean rebuilding
//! it from the log, which is not built; refusing keeps the store as it is
//! written equal to the store as the log replays.

use std::collections::{BTreeMap, HashSet};

use crate::log::Engine;

#[derive(Debug, Default)]
pub(super) struct Tables {
    declared: BTreeMap<Vec<u8>, Engine>,
    /// Tables some commit has written to, on any branch, including keys
    /// removed since.
    written: HashSet<Vec<u8>>,
}

/// Why a table cannot be declared.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// Empty, or more than one part of a key.
    BadName,
    InUse,
}

impl Tables {
    pub fn declared(&self) -> &BTreeMap<Vec<u8>, Engine> {
        &self.declared
    }

    /// The table of `key` when that table keeps only latest values.
    pub fn latest_table_of<'k>(&self, key: &'k [u8]) -> Option<&'k [u8]> {
        table_of(key).filter(|table| self.declared.get(*table) == Some(&Engine::Latest))
    }

    pub fn engine_of(&self, key: &[u8]) -> Engine {
        table_of(key)
            .and_then(|table| self.declared.get(table))
            .copied()
            .unwrap_or(Engine::History)
    }

    /// Whether `name` may be declared now.
    pub fn check(&self, name: &[u8]) -> Result<(), Refusal> {
        if name.is_empty() || name.contains(&b'/') {
            return Err(Refusal::BadName);
        }
        if self.written.contains(name) {
            return Err(Refusal::InUse);
        }
        Ok(())
    }

    /// Records a declaration the log holds. The last one of a table wins.
    pub fn declare(&mut self, name: &[u8], engine: Engine) -> Result<(), Refusal> {
        self.check(name)?;
        self.declared.insert(name.to_vec(), engine);
        Ok(())
    }

    /// Notes that a commit wrote `key`. Allocates only for a table not seen
    /// before, so a write to a known table costs a lookup.
    pub fn note_written(&mut self, key: &[u8]) {
        if let Some(table) = table_of(key)
            && !self.written.contains(table)
        {
            self.written.insert(table.to_vec());
        }
    }
}

/// The table `key` is in, or `None` for a key of one part.
fn table_of(key: &[u8]) -> Option<&[u8]> {
    let at = key.iter().position(|byte| *byte == b'/')?;
    Some(&key[..at])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_is_the_first_part_of_a_key() {
        assert_eq!(table_of(b"player/42/gems"), Some(&b"player"[..]));
        assert_eq!(table_of(b"player/42"), Some(&b"player"[..]));
        assert_eq!(table_of(b"/odd"), Some(&b""[..]));
        assert_eq!(table_of(b"config"), None);
    }

    #[test]
    fn a_written_table_is_refused_and_others_are_not() {
        let mut tables = Tables::default();
        tables.note_written(b"player/1/gold");

        assert_eq!(
            tables.declare(b"player", Engine::Latest),
            Err(Refusal::InUse)
        );
        assert_eq!(tables.declare(b"position", Engine::Latest), Ok(()));
        assert_eq!(tables.engine_of(b"position/1/x"), Engine::Latest);
        assert_eq!(tables.engine_of(b"player/1/gold"), Engine::History);
    }
}
