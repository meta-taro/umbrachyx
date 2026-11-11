//! Taking in another store's log: what a replica does with the bytes its
//! primary sends.
//!
//! The log is the truth, and a commit's id is a hash of what the log holds
//! of it, so a store that applies the same records in the same order holds
//! the same commits under the same ids. Nothing else has to be sent: the
//! trees, the cells and the decisions are rebuilt here as they are when a
//! store opens its own log.
//!
//! Values stay sealed on the way and in the replica's log. To read them
//! the replica needs the keys the primary sealed them under; how those
//! reach it is not decided here, and records whose keys it lacks are
//! refused rather than taken in unreadable, which would look erased.

use super::{Clock, Store, StoreError, check_sealed};
use crate::log::Storage;
use crate::log::record::{self, Record};

impl<S: Storage, C: Clock> Store<S, C> {
    /// Applies the whole records at the start of `frames`, bytes of a
    /// primary's log from where this store's log ends, and returns how
    /// many bytes they were. A record cut short at the end is left for
    /// the next call, with the bytes that complete it.
    ///
    /// Each record is written to this store's log and synced before it is
    /// applied, as its own records are. An erasure destroys this store's
    /// copy of the key too. Damaged records, and records sealed under keys
    /// this store does not hold, are refused before anything is written.
    pub fn replicate(&mut self, frames: &[u8]) -> Result<usize, StoreError> {
        let scan = record::scan(frames)?;
        if let Some(sealer) = &self.sealer {
            check_sealed(&scan.records, &sealer.ring())?;
        }
        for record in &scan.records {
            self.log.append(record)?;
            self.apply(record)
                .map_err(|error| StoreError::Inconsistent(error.to_string()))?;
            if let (Record::Erase(erase), Some(sealer)) = (record, &mut self.sealer) {
                sealer.erase_fingerprint(&erase.key_fingerprint)?;
            }
        }
        Ok(scan.valid_len as usize)
    }
}
