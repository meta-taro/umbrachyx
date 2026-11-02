//! Umbrachyx: an in-memory database that keeps every change as history.
//!
//! The append-only log, the tree and a prototype of capability tokens exist
//! so far. See `docs/concept.md` for what this crate is meant to become.

pub mod capability;
pub mod log;
#[cfg(test)]
mod sim;
pub mod tree;
