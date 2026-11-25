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

use std::collections::BTreeMap;

use super::follow::KeyAnswer;
use super::seal::Ring;
use super::{Clock, Store, StoreError, check_sealed, keys_lacked};
use crate::keys::{Fingerprint, Key, fingerprint};
use crate::log::record::{self, Record};
use crate::log::{Chain, LogError, Storage};

impl<S: Storage, C: Clock> Store<S, C> {
    /// How many records this store's log holds. A replica asks its
    /// primary for what comes after as many.
    pub fn log_records(&self) -> u64 {
        self.log.records()
    }

    /// A hash of the first `records` records of this store's log, in
    /// order, or `None` if it holds fewer. Before a primary sends a
    /// replica what follows the records it holds, the two compare this:
    /// if it differs, the replica holds another log, and what follows
    /// would be appended to the wrong history.
    pub fn log_chain(&mut self, records: u64) -> Result<Option<Chain>, StoreError> {
        self.log
            .chain_at(records)
            .map_err(|error| LogError::from(error).into())
    }

    /// The bytes of this store's log after its first `records` records:
    /// whole records, as many as fit in `max_bytes` and at least one if
    /// there is one. Only records already synced are handed out, so a
    /// replica never holds what its primary could still lose.
    pub fn log_after(&mut self, records: u64, max_bytes: usize) -> Result<Vec<u8>, StoreError> {
        self.log
            .read_after(records, max_bytes)
            .map_err(|error| LogError::from(error).into())
    }

    /// Applies the whole records at the start of `frames`, bytes of a
    /// primary's log from where this store's log ends, and returns how
    /// many bytes they were. A record cut short at the end is left for
    /// the next call, with the bytes that complete it.
    ///
    /// The records are written to this store's log and synced, all with
    /// one sync, before any is applied. An erasure destroys this store's
    /// copy of the key too. Damaged records, and records sealed under keys
    /// this store does not hold, are refused before anything is written;
    /// the records up to an erasure are taken in without the rest if
    /// only those lack no key (see [`readable`]).
    ///
    /// One error comes with the records taken in: the key file could not
    /// be written to finish an erasure among them. The key is out of
    /// use from then on, and the file is finished when the store next
    /// opens.
    pub fn replicate(&mut self, frames: &[u8]) -> Result<usize, StoreError> {
        let scan = record::scan(frames)?;
        if scan.records.is_empty() {
            return Ok(0);
        }
        let take = match &self.sealer {
            Some(sealer) => readable(&scan.records, &sealer.ring())?,
            None => scan.records.len(),
        };
        let whole = scan.ends[take - 1] as usize;
        let before = self.log.records();
        self.sync_puts()?;
        self.log
            .append_frames(&frames[..whole], &scan.ends[..take])?;
        // The records are in the log, so every one is applied whatever
        // happens to the key file: an erasure whose key the file still
        // holds is out of the ring already, and is finished when the
        // store next opens, as one of this store's own would be.
        let mut unfinished = None;
        for (at, record) in scan.records[..take].iter().enumerate() {
            if let Record::Erase(erase) = record {
                self.note_erased(erase.key_fingerprint, before + at as u64 + 1);
            }
            self.apply(record)
                .map_err(|error| StoreError::Inconsistent(error.to_string()))?;
            if let (Record::Erase(erase), Some(sealer)) = (record, &mut self.sealer)
                && let Err(error) = sealer.erase_fingerprint(&erase.key_fingerprint)
            {
                unfinished.get_or_insert(error);
            }
        }
        match unfinished {
            Some(error) => Err(error.into()),
            None => Ok(whole),
        }
    }

    /// What this store says of the key with each of these fingerprints:
    /// the key, that its log holds the erasure that destroyed it, or
    /// that it never held one.
    pub(super) fn keys_wanted(&self, wanted: &[Fingerprint]) -> Vec<KeyAnswer> {
        wanted
            .iter()
            .map(|wanted| {
                let key = self
                    .sealer
                    .as_ref()
                    .and_then(|sealer| sealer.key_of(wanted));
                match key {
                    Some(key) => KeyAnswer::Key(*key),
                    None => match self.erased_keys.get(wanted) {
                        Some(at) => KeyAnswer::Erased {
                            key: *wanted,
                            at: *at,
                        },
                        None => KeyAnswer::Unknown(*wanted),
                    },
                }
            })
            .collect()
    }

    /// The keys this store lacks for the whole records at the start of
    /// `frames`, each with its subject. A store that does not encrypt
    /// lacks none.
    pub(super) fn keys_lacked_for(
        &self,
        frames: &[u8],
    ) -> Result<BTreeMap<Fingerprint, Vec<u8>>, StoreError> {
        let Some(sealer) = &self.sealer else {
            return Ok(BTreeMap::new());
        };
        keys_lacked(&record::scan(frames)?.records, &sealer.ring())
    }

    /// Keeps those of `keys` that the records at the start of `frames`
    /// are sealed under and this store lacks, each for the subject the
    /// records name. Durable before this returns. Any other key is left.
    pub(super) fn keep_keys_for(&mut self, frames: &[u8], keys: &[Key]) -> Result<(), StoreError> {
        let lacked = self.keys_lacked_for(frames)?;
        let Some(sealer) = &mut self.sealer else {
            return Ok(());
        };
        let needed: Vec<(&[u8], Key)> = keys
            .iter()
            .filter_map(|key| {
                let subject = lacked.get(&fingerprint(key))?;
                Some((subject.as_slice(), *key))
            })
            .collect();
        sealer.take_keys(&needed)?;
        Ok(())
    }
}

/// How many of `records`, from the first, a store with the keys in
/// `ring` can take in: all of them, or else as many as end with an
/// erasure and lack no key by themselves.
///
/// The second is for a subject erased and written again under a new key,
/// both in one answer. A subject has one key, so the new one cannot be
/// kept while this store still holds the old; once the records up to the
/// erasure are in, it can, and the rest is asked for again. Cutting
/// anywhere else could leave values here whose key was never held and
/// whose erasure is not in this log, which would not open again. For the
/// same reason no cut will do if the records before it need a key that
/// an erasure after it destroyed: the store is then stopped by a key
/// destroyed before it had it, and says that.
fn readable(records: &[Record], ring: &Ring) -> Result<usize, StoreError> {
    let Err(lacked) = check_sealed(records, ring) else {
        return Ok(records.len());
    };
    let erased: Vec<(usize, Fingerprint)> = records
        .iter()
        .enumerate()
        .filter_map(|(at, record)| match record {
            Record::Erase(erase) => Some((at + 1, erase.key_fingerprint)),
            _ => None,
        })
        .collect();
    let mut destroyed_first = 0;
    for (take, _) in erased
        .iter()
        .rev()
        .filter(|(take, _)| *take < records.len())
    {
        let lacked = keys_lacked(&records[..*take], ring)?;
        if lacked.is_empty() {
            return Ok(*take);
        }
        // The cut would do, but for keys an erasure after it destroyed:
        // nobody has those to give.
        let is_destroyed = |key| erased.iter().any(|(_, erased)| erased == key);
        if destroyed_first == 0 && lacked.keys().all(is_destroyed) {
            destroyed_first = lacked.len();
        }
    }
    match destroyed_first {
        0 => Err(lacked),
        keys => Err(StoreError::KeysErased(keys)),
    }
}
