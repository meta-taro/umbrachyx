//! Deterministic simulation: every source of nondeterminism the log sees is
//! replaced by something a seed controls, so a failure found once can be
//! replayed exactly.
//!
//! Only the disk is simulated so far, because the log is the only component
//! and it does nothing with time or the network.

mod disk;
mod rng;
mod run;

pub use disk::SimStorage;
