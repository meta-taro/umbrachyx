//! An in-memory [`Storage`] that can crash.
//!
//! It keeps synced and unsynced bytes apart, so a test can throw away what a
//! power cut would lose, keep a torn part of it, or make the next sync fail.
//! This is the seed of the deterministic simulation disk.

use std::io;

use super::storage::Storage;

#[derive(Debug, Default, Clone)]
pub struct SimStorage {
    durable: Vec<u8>,
    pending: Vec<u8>,
    is_next_sync_failing: bool,
}

impl SimStorage {
    /// What the disk holds after a power cut: the synced bytes, plus the
    /// first `torn` bytes of whatever had not been synced yet.
    pub fn crash(&self, torn: usize) -> Self {
        let kept = torn.min(self.pending.len());
        Self {
            durable: [self.durable.as_slice(), &self.pending[..kept]].concat(),
            ..Self::default()
        }
    }

    pub fn fail_next_sync(&mut self) {
        self.is_next_sync_failing = true;
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn with_bytes(bytes: Vec<u8>) -> Self {
        Self {
            durable: bytes,
            ..Self::default()
        }
    }
}

impl Storage for SimStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok([self.durable.as_slice(), &self.pending].concat())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    /// A failed sync drops the unsynced bytes, the way Linux may drop dirty
    /// pages after a writeback error and still report them clean later.
    fn sync(&mut self) -> io::Result<()> {
        if self.is_next_sync_failing {
            self.is_next_sync_failing = false;
            self.pending.clear();
            return Err(io::Error::other("injected sync failure"));
        }
        self.durable.append(&mut self.pending);
        Ok(())
    }

    /// The cut is not durable until the next sync: the durable bytes shrink
    /// only as far as they must, and what remains stays pending.
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        let all = self.read_all()?;
        let kept = &all[..len.min(all.len())];
        let durable_len = self.durable.len().min(kept.len());
        self.durable.truncate(durable_len);
        self.pending = kept[durable_len..].to_vec();
        Ok(())
    }
}
