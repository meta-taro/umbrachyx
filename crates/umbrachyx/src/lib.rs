//! Umbrachyx: an in-memory database that keeps every change as history.
//!
//! The append-only log, the tree, the store that joins them, a server that
//! speaks the Redis protocol for one branch and reads the history of any, capability tokens, and an MCP
//! session that lets an agent read and write within its token exist so far. See `docs/concept.md` for what this crate is meant to become.

pub mod capability;
pub mod hamt;
pub mod keys;
pub mod log;
pub mod mcp;
mod report;
pub mod server;
#[cfg(test)]
mod sim;
pub mod store;
pub mod tree;
