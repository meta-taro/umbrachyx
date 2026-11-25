//! Checking a copy of a log's file a part at a time.
//!
//! A backup's copy of the log can be far larger than the memory of the
//! machine that restores it. It is read in parts, and what is kept of
//! them is the part being read and the record that runs across its end:
//! one record at most, however long the log.

use super::header::{self, Start};
use super::record::{self, Next};
use super::{Chain, HEADER_LEN, LogError, LogId, chained};

/// What a copy of a log's file turned out to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Copied {
    pub(crate) id: LogId,
    pub(crate) records: u64,
    /// The chain over all of them.
    pub(crate) chain: Chain,
    /// A hash of every byte, in order: what reading the copy again must
    /// give for it to be the copy that was checked.
    pub(crate) digest: [u8; 32],
}

/// What the header at the start of the copy said.
struct Started {
    id: LogId,
    chain: Chain,
    records: u64,
}

/// A copy of a log's file from its start, checked as its bytes are fed
/// in order. Every byte must belong: a copy that ends in part of a record
/// is refused, as damage inside it is, and every record must decode.
pub(crate) struct CopyCheck {
    /// `None` until the whole header has been fed.
    started: Option<Started>,
    /// Bytes fed and not yet checked: less than a header, or the start
    /// of a record whose end has not come.
    held: Vec<u8>,
    /// Where `held` starts in the copy.
    offset: u64,
    digest: blake3::Hasher,
}

impl CopyCheck {
    pub(crate) fn new() -> Self {
        Self {
            started: None,
            held: Vec::new(),
            offset: 0,
            digest: blake3::Hasher::new(),
        }
    }

    /// The next bytes of the copy, however many.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<(), LogError> {
        self.digest.update(bytes);
        self.held.extend_from_slice(bytes);
        let mut checked = 0;
        if self.started.is_none() {
            if self.held.len() < HEADER_LEN {
                return Ok(());
            }
            self.started = Some(start(&self.held)?);
            checked = HEADER_LEN;
        }
        let started = self.started.as_mut().expect("set above");
        loop {
            let rest = &self.held[checked..];
            let at = self.offset + checked as u64;
            let corrupt = |reason| LogError::Corrupt { offset: at, reason };
            let end = match record::next_frame(rest).map_err(corrupt)? {
                Next::Frame(end) => end,
                Next::Short => break,
            };
            let frame = &rest[..end];
            if record::decode_frame(frame).is_none() {
                return Err(corrupt("checksummed payload does not decode"));
            }
            started.chain = chained(&started.chain, frame);
            started.records += 1;
            checked += end;
        }
        self.held.drain(..checked);
        self.offset += checked as u64;
        Ok(())
    }

    /// What the copy held, once all of it has been fed.
    pub(crate) fn finish(self) -> Result<Copied, LogError> {
        let Some(started) = self.started else {
            // Shorter than a header: said as a header of that length is.
            return start(&self.held).map(|_| unreachable!("a header is longer"));
        };
        if !self.held.is_empty() {
            return Err(LogError::Corrupt {
                offset: self.offset,
                reason: "a record cut short",
            });
        }
        Ok(Copied {
            id: started.id,
            records: started.records,
            chain: started.chain,
            digest: *self.digest.finalize().as_bytes(),
        })
    }
}

/// The header `bytes` start with: that of the first file of a log.
fn start(bytes: &[u8]) -> Result<Started, LogError> {
    let header = match header::read(bytes) {
        Start::Header(header) => header,
        Start::Newer(version) => return Err(LogError::NewerFormat { version }),
        Start::None => {
            return Err(LogError::Corrupt {
                offset: 0,
                reason: "no log header",
            });
        }
    };
    if header.first_record != 0 {
        return Err(LogError::Corrupt {
            offset: 0,
            reason: "the file is not the first of its log",
        });
    }
    Ok(Started {
        id: header.id,
        chain: header.chain_before,
        records: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::record::{Commit, Edit, Record};
    use crate::log::{Log, Storage};
    use crate::sim::SimStorage;

    /// The file of a log that holds a few commits of synthetic players.
    fn file() -> Vec<u8> {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for player in 0..4_u8 {
            let record = Record::Commit(Commit {
                branch: "main".to_owned(),
                changes: vec![Edit {
                    key: format!("player/{player}/gems").into_bytes(),
                    value: Some(vec![player; 3 + usize::from(player) * 7]),
                }],
                who: "test".to_owned(),
                why: "a synthetic player".to_owned(),
                approved_by: None,
                time: u64::from(player),
                merged: None,
            });
            log.append(&record).expect("appends");
        }
        log.storage.read_all().expect("reads")
    }

    fn fed_in_parts(bytes: &[u8], part: usize) -> Result<Copied, LogError> {
        let mut check = CopyCheck::new();
        for part in bytes.chunks(part) {
            check.feed(part)?;
        }
        check.finish()
    }

    /// The copy checked all at once, as it was before it was read in
    /// parts: what any way of cutting it into parts must agree with.
    fn at_once(bytes: &[u8]) -> Result<(LogId, u64, Chain), LogError> {
        crate::log::check_copy_at_once(bytes)
    }

    fn agrees(bytes: &[u8]) {
        let whole = at_once(bytes);
        for part in [1, 2, 3, 7, 64, bytes.len().max(1)] {
            let parts = fed_in_parts(bytes, part);
            match (&whole, &parts) {
                (Ok(whole), Ok(parts)) => {
                    assert_eq!(*whole, (parts.id, parts.records, parts.chain));
                }
                (Err(_), Err(_)) => {}
                _ => panic!(
                    "in parts of {part}: {parts:?}, at once: {whole:?}, of {} bytes",
                    bytes.len()
                ),
            }
        }
    }

    #[test]
    fn a_good_copy_holds_the_same_however_it_is_cut_into_parts() {
        let bytes = file();

        let whole = at_once(&bytes).expect("checks out");
        agrees(&bytes);

        assert_eq!(whole.1, 4);
    }

    #[test]
    fn the_digest_is_of_the_bytes_and_not_of_how_they_were_cut() {
        let bytes = file();

        let by_one = fed_in_parts(&bytes, 1).expect("checks out");
        let by_many = fed_in_parts(&bytes, 50).expect("checks out");

        assert_eq!(by_one, by_many);
        assert_eq!(by_one.digest, *blake3::hash(&bytes).as_bytes());
    }

    #[test]
    fn every_changed_byte_is_refused_however_the_copy_is_cut() {
        let bytes = file();

        for at in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[at] ^= 0x01;
            assert!(at_once(&changed).is_err(), "byte {at} at once");
            agrees(&changed);
        }
    }

    #[test]
    fn every_copy_cut_short_is_refused_unless_it_ends_with_a_record() {
        let bytes = file();

        for len in 0..bytes.len() {
            agrees(&bytes[..len]);
        }
    }

    #[test]
    fn a_length_no_record_has_is_refused_before_its_bytes_are_waited_for() {
        let mut bytes = file()[..HEADER_LEN].to_vec();
        // A frame that says it is longer than any record can be.
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);

        let mut check = CopyCheck::new();
        let refused = check.feed(&bytes);

        assert!(refused.is_err(), "{refused:?}");
    }
}
