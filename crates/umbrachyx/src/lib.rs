//! Umbrachyx: an in-memory database that keeps every change as history.
//!
//! The append-only log, the tree, the store that joins them, a server that
//! speaks the Redis protocol for one branch, and a prototype of capability
//! tokens exist so far. See `docs/concept.md` for what this crate is meant to become.

pub mod capability;
pub mod log;
pub mod server;
#[cfg(test)]
mod sim;
pub mod store;
pub mod tree;
