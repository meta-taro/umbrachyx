//! An in-memory [`KeyStorage`] whose next write can fail.
//!
//! Every write to the key file is durable when it returns, so there is
//! nothing for a power cut to lose between writes. What can go wrong is a
//! write itself: an append cut short leaves part of its entry at the end,
//! and a replacement that fails leaves the old file or, if the rename got
//! through before the error, the new one.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::rng::Rng;
use crate::keys::KeyStorage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFault {
    /// How many bytes of a failed append reach the disk, at most.
    pub torn: usize,
    /// If set, those bytes come out as garbage drawn from this seed rather
    /// than as written.
    pub garbage: Option<u64>,
    /// Whether a failed replacement leaves the new file in place.
    pub new_survives: bool,
}

#[derive(Debug, Default)]
struct KeyDisk {
    bytes: Vec<u8>,
    next_fault: Option<KeyFault>,
}

/// Clones share one disk, so the simulation keeps a handle while the store
/// owns another, and the disk outlives a store that crashed.
#[derive(Debug, Default, Clone)]
pub struct SimKeyStorage(Arc<Mutex<KeyDisk>>);

impl SimKeyStorage {
    fn disk(&self) -> MutexGuard<'_, KeyDisk> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn fail_next_write(&self, fault: KeyFault) {
        self.disk().next_fault = Some(fault);
    }

    /// Takes back a fault no write used, so it cannot hit a later one.
    /// Returns whether one was still waiting.
    pub fn clear_fault(&self) -> bool {
        self.disk().next_fault.take().is_some()
    }
}

impl KeyStorage for SimKeyStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.disk().bytes.clone())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut disk = self.disk();
        let Some(fault) = disk.next_fault.take() else {
            disk.bytes.extend_from_slice(bytes);
            return Ok(());
        };
        let kept = fault.torn.min(bytes.len());
        if let Some(seed) = fault.garbage {
            disk.bytes.extend(Rng::new(seed).bytes(kept));
        } else {
            disk.bytes.extend_from_slice(&bytes[..kept]);
        }
        Err(io::Error::other("injected key file failure, append torn"))
    }

    fn replace(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut disk = self.disk();
        let Some(fault) = disk.next_fault.take() else {
            disk.bytes = bytes.to_vec();
            return Ok(());
        };
        if fault.new_survives {
            disk.bytes = bytes.to_vec();
        }
        Err(io::Error::other("injected key file failure, replace"))
    }
}
