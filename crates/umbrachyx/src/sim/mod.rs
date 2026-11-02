//! Deterministic simulation: every source of nondeterminism the database
//! sees is replaced by something a seed controls, so a failure found once
//! can be replayed exactly.
//!
//! The disk and the clock are simulated. There is no network yet.

mod disk;
mod rng;
mod run;
mod store_run;

pub use disk::SimStorage;
