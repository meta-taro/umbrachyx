//! Deterministic simulation: every source of nondeterminism the database
//! sees is replaced by something a seed controls, so a failure found once
//! can be replayed exactly.
//!
//! The disk and the clock are simulated, and so is the place a backup is
//! kept, and the network between a replica and its primary.

mod backup_run;
mod disk;
mod key_disk;
mod keys_run;
mod net;
mod plain_run;
mod rng;
mod run;
mod sealed_run;
mod store_run;

pub use disk::SimStorage;
