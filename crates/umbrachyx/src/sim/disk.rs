//! An in-memory [`Storage`] that can crash.
//!
//! It keeps synced and unsynced bytes apart, so a test can throw away what a
//! power cut would lose, tear what was in flight, fail a sync, or flip a bit
//! in data that was already durable.

use std::io;

use super::rng::Rng;
use crate::log::Storage;

/// Disks write in sectors, and a power cut can leave each sector of an
/// unfinished write in any state, not only a clean prefix.
const SECTOR_LEN: usize = 512;

/// Out of 100: what a power cut leaves in each sector of an unsynced write.
const SECTOR_KEPT_PERCENT: u64 = 80;
const SECTOR_ZEROED_PERCENT: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFault {
    /// The sync fails and the unsynced bytes are gone, the way Linux may drop
    /// dirty pages after a writeback error.
    DropPending,
    /// The sync fails and the bytes stay unsynced: power was cut while the
    /// sync was in flight.
    KeepPending,
}

/// What the disk durably holds and what the process currently sees are kept
/// apart. Appends and truncates change only what is seen; a sync makes it
/// durable. A process crash keeps what is seen (the kernel still has it); a
/// power cut keeps only what is durable, plus whatever of the unsynced change
/// the disk happened to write.
#[derive(Debug, Default, Clone)]
pub struct SimStorage {
    durable: Vec<u8>,
    current: Vec<u8>,
    next_sync_fault: Option<SyncFault>,
}

impl SimStorage {
    /// What the disk holds after a power cut: the synced bytes, plus the
    /// first `torn` bytes of whatever had been appended since.
    pub fn crash(&self, torn: usize) -> Self {
        let pending = self.appended_since_sync().unwrap_or_default();
        let kept = torn.min(pending.len());
        Self::with_bytes([self.durable.as_slice(), &pending[..kept]].concat())
    }

    /// A power cut. Bytes appended since the last sync are cut at a random
    /// length and each sector of what is left is kept, zeroed or filled with
    /// garbage. A truncate since the last sync either happened or did not.
    pub fn power_cut(&self, rng: &mut Rng) -> Self {
        let Some(pending) = self.appended_since_sync() else {
            let survivor = if rng.percent(50) {
                &self.durable
            } else {
                &self.current
            };
            return Self::with_bytes(survivor.clone());
        };
        let cut = rng.below(pending.len() as u64 + 1) as usize;
        let mut torn = pending[..cut].to_vec();
        let mut start = 0;
        while start < torn.len() {
            // Sectors are aligned to the file, not to the start of the write.
            let offset = self.durable.len() + start;
            let end = (start + SECTOR_LEN - offset % SECTOR_LEN).min(torn.len());
            let roll = rng.below(100);
            if roll >= SECTOR_KEPT_PERCENT + SECTOR_ZEROED_PERCENT {
                torn[start..end].copy_from_slice(&rng.bytes(end - start));
            } else if roll >= SECTOR_KEPT_PERCENT {
                torn[start..end].fill(0);
            }
            start = end;
        }
        Self::with_bytes([self.durable.as_slice(), &torn].concat())
    }

    /// The process dies but the machine does not: everything written stays
    /// readable, and nothing becomes more durable than it was.
    pub fn process_crash(&self) -> Self {
        Self {
            durable: self.durable.clone(),
            current: self.current.clone(),
            next_sync_fault: None,
        }
    }

    /// Bytes appended since the last sync, or `None` if the file was cut
    /// shorter than its durable length since then.
    fn appended_since_sync(&self) -> Option<&[u8]> {
        self.current
            .starts_with(&self.durable)
            .then(|| &self.current[self.durable.len()..])
    }

    pub fn fail_next_sync(&mut self) {
        self.next_sync_fault = Some(SyncFault::DropPending);
    }

    pub fn fault_next_sync(&mut self, fault: SyncFault) {
        self.next_sync_fault = Some(fault);
    }

    /// Takes back a fault no sync used, so it cannot hit a later one.
    pub fn clear_sync_fault(&mut self) {
        self.next_sync_fault = None;
    }

    /// Flip one bit on the disk: damage after acknowledgement. Only for a
    /// disk fresh from a power cut, where what is seen is what is durable.
    pub fn flip_bit(&mut self, bit: usize) {
        debug_assert_eq!(self.durable, self.current);
        self.durable[bit / 8] ^= 1 << (bit % 8);
        self.current[bit / 8] ^= 1 << (bit % 8);
    }

    pub fn durable_len(&self) -> usize {
        self.durable.len()
    }

    /// Bytes that would not survive a power cut as they are seen now.
    pub fn pending_len(&self) -> usize {
        match self.appended_since_sync() {
            Some(pending) => pending.len(),
            None => self.current.len(),
        }
    }

    pub fn with_bytes(bytes: Vec<u8>) -> Self {
        Self {
            durable: bytes.clone(),
            current: bytes,
            next_sync_fault: None,
        }
    }
}

impl Storage for SimStorage {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.current.clone())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.current.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        match self.next_sync_fault.take() {
            Some(SyncFault::DropPending) => {
                self.current = self.durable.clone();
                Err(io::Error::other(
                    "injected sync failure, unsynced bytes dropped",
                ))
            }
            Some(SyncFault::KeepPending) => Err(io::Error::other(
                "injected sync failure, power cut in flight",
            )),
            None => {
                self.durable = self.current.clone();
                Ok(())
            }
        }
    }

    /// Not durable until the next sync, like any other change.
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.current.truncate(len);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk_with_pending(durable: usize, pending: usize) -> SimStorage {
        let mut disk = SimStorage::with_bytes(vec![0xD0; durable]);
        disk.append(&vec![0xAA; pending]).expect("appends");
        disk
    }

    #[test]
    fn power_cut_never_touches_durable_bytes() {
        let disk = disk_with_pending(700, 5_000);

        for seed in 0..200 {
            let mut after = disk.power_cut(&mut Rng::new(seed));
            let bytes = after.read_all().expect("reads");

            assert!(bytes.len() >= 700 && bytes.len() <= 5_700, "seed {seed}");
            assert!(bytes[..700].iter().all(|&b| b == 0xD0), "seed {seed}");
            assert_eq!(after.pending_len(), 0, "seed {seed}");
        }
    }

    #[test]
    fn power_cut_is_the_same_for_the_same_seed() {
        let disk = disk_with_pending(100, 3_000);

        let mut first = disk.power_cut(&mut Rng::new(7));
        let mut second = disk.power_cut(&mut Rng::new(7));

        assert_eq!(
            first.read_all().expect("reads"),
            second.read_all().expect("reads")
        );
    }

    #[test]
    fn power_cut_tears_sectors_out_of_order() {
        let disk = disk_with_pending(0, 8 * SECTOR_LEN);

        let is_zeroed_before_kept = (0..500).any(|seed| {
            let bytes = disk
                .power_cut(&mut Rng::new(seed))
                .read_all()
                .expect("reads");
            let sectors: Vec<&[u8]> = bytes.chunks(SECTOR_LEN).collect();
            sectors
                .windows(2)
                .any(|pair| pair[0].iter().all(|&b| b == 0) && pair[1].iter().all(|&b| b == 0xAA))
        });

        assert!(
            is_zeroed_before_kept,
            "only clean prefixes were ever produced"
        );
    }

    #[test]
    fn truncate_is_not_durable_until_synced() {
        let mut disk = SimStorage::with_bytes(vec![1; 100]);
        disk.truncate(40).expect("truncates");

        let lengths: Vec<usize> = (0..50)
            .map(|seed| disk.power_cut(&mut Rng::new(seed)).durable_len())
            .collect();
        disk.sync().expect("syncs");

        assert!(
            lengths.contains(&100) && lengths.contains(&40),
            "{lengths:?}"
        );
        assert_eq!(disk.power_cut(&mut Rng::new(0)).durable_len(), 40);
    }

    #[test]
    fn process_crash_keeps_unsynced_bytes_readable_but_not_durable() {
        let disk = disk_with_pending(10, 20);

        let mut after = disk.process_crash();

        assert_eq!(after.read_all().expect("reads").len(), 30);
        assert_eq!(after.pending_len(), 20);
    }

    #[test]
    fn sync_fault_keep_pending_leaves_bytes_unsynced() {
        let mut disk = disk_with_pending(0, 10);
        disk.fault_next_sync(SyncFault::KeepPending);

        let result = disk.sync();

        assert!(result.is_err());
        assert_eq!(disk.pending_len(), 10);
        assert_eq!(disk.durable_len(), 0);
    }
}
