//! The append-only log: the one source of truth.
//!
//! A write is acknowledged only after its record is synced to storage. The
//! tree and the history are rebuilt from this log, so if the log is wrong,
//! everything above it is wrong too.

pub(crate) mod record;
mod storage;

use std::fmt;
use std::io;

pub use record::{Edit, MAX_PAYLOAD_LEN, Record};
pub use storage::{FileStorage, Storage};

#[derive(Debug)]
pub enum LogError {
    Io(io::Error),
    /// Bytes that had been acknowledged no longer check out. The log refuses
    /// to open rather than silently dropping them.
    Corrupt {
        offset: u64,
        reason: &'static str,
    },
    RecordTooLarge {
        len: usize,
    },
    /// An earlier append or sync failed. After a failed sync the state of the
    /// unsynced bytes is unknown, so retrying could acknowledge lost data.
    Poisoned,
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "log storage failed: {error}"),
            Self::Corrupt { offset, reason } => {
                write!(f, "log is corrupt at byte {offset}: {reason}")
            }
            Self::RecordTooLarge { len } => write!(
                f,
                "record payload is {len} bytes, more than the {MAX_PAYLOAD_LEN} allowed"
            ),
            Self::Poisoned => write!(f, "log stopped accepting writes after a failed write"),
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LogError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub struct Log<S: Storage> {
    storage: S,
    len: u64,
    is_poisoned: bool,
}

impl<S: Storage> Log<S> {
    /// Open the log and return every acknowledged record in order. A torn
    /// tail left by an interrupted write is cut off before anything new is
    /// appended.
    pub fn open(mut storage: S) -> Result<(Self, Vec<Record>), LogError> {
        let bytes = storage.read_all()?;
        let scan = record::scan(&bytes)?;
        if scan.valid_len < bytes.len() as u64 {
            storage.truncate(scan.valid_len)?;
        }
        // Always sync: a process that died between write and sync leaves
        // records the kernel still serves but the disk may not hold. Handing
        // them out before they are durable would acknowledge them twice over.
        storage.sync()?;
        let log = Self {
            storage,
            len: scan.valid_len,
            is_poisoned: false,
        };
        Ok((log, scan.records))
    }

    /// Append one record and return only once it is durable.
    pub fn append(&mut self, record: &Record) -> Result<(), LogError> {
        if self.is_poisoned {
            return Err(LogError::Poisoned);
        }
        let frame = record::encode(record)?;
        if let Err(error) = self.write_durably(&frame) {
            self.is_poisoned = true;
            return Err(error.into());
        }
        self.len += frame.len() as u64;
        Ok(())
    }

    fn write_durably(&mut self, frame: &[u8]) -> io::Result<()> {
        self.storage.append(frame)?;
        self.storage.sync()
    }

    /// Bytes of acknowledged records in the log.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The storage underneath, for the simulation to inject faults into.
    #[cfg(test)]
    pub(crate) fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimStorage;

    fn record(n: u32) -> Record {
        Record {
            branch: "main".to_owned(),
            changes: vec![Edit {
                key: format!("player:{n}:coins").into_bytes(),
                value: Some(n.to_le_bytes().to_vec()),
            }],
            who: "agent:shop".to_owned(),
            why: "purchase".to_owned(),
            approved_by: None,
            time: u64::from(n),
        }
    }

    fn written(records: &[Record]) -> SimStorage {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for r in records {
            log.append(r).expect("appends");
        }
        log.storage
    }

    #[test]
    fn acknowledged_records_survive_a_crash() {
        let storage = written(&[record(1), record(2), record(3)]);

        let (_, recovered) = Log::open(storage.crash(0)).expect("reopens");

        assert_eq!(recovered, vec![record(1), record(2), record(3)]);
    }

    #[test]
    fn append_does_not_return_before_the_record_is_synced() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");

        log.append(&record(1)).expect("appends");

        assert_eq!(log.storage.pending_len(), 0);
    }

    #[test]
    fn torn_tail_is_cut_and_writing_continues_after_it() {
        let durable = written(&[record(1)]);
        let mut torn = durable.clone();
        torn.append(&record::encode(&record(2)).expect("encodes"))
            .expect("appends");
        let crashed = torn.crash(5);

        let (mut log, recovered) = Log::open(crashed).expect("reopens");
        log.append(&record(3)).expect("appends");
        let (_, after) = Log::open(log.storage.crash(0)).expect("reopens");

        assert_eq!(recovered, vec![record(1)]);
        assert_eq!(after, vec![record(1), record(3)]);
    }

    #[test]
    fn open_makes_records_it_returns_durable() {
        // A previous process wrote this frame and died before syncing it. The
        // kernel still serves it, so recovery must not hand it out until it
        // is on disk.
        let mut storage = written(&[record(1)]);
        storage
            .append(&record::encode(&record(2)).expect("encodes"))
            .expect("appends");

        let (log, recovered) = Log::open(storage).expect("reopens");

        assert_eq!(recovered, vec![record(1), record(2)]);
        assert_eq!(log.storage.pending_len(), 0);
    }

    #[test]
    fn damaged_acknowledged_record_refuses_to_open() {
        let mut bytes = written(&[record(1), record(2)]).read_all().expect("reads");
        bytes[record::HEADER_LEN + 1] ^= 0x80;

        let error = Log::open(SimStorage::with_bytes(bytes)).expect_err("corrupt");

        assert!(matches!(error, LogError::Corrupt { .. }), "{error:?}");
    }

    #[test]
    fn failed_sync_is_reported_and_poisons_the_log() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        log.storage.fail_next_sync();

        let failed = log.append(&record(1));
        let after = log.append(&record(2));

        assert!(matches!(failed, Err(LogError::Io(_))), "{failed:?}");
        assert!(matches!(after, Err(LogError::Poisoned)), "{after:?}");
    }

    #[test]
    fn oversized_record_is_refused_without_poisoning() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        let mut big = record(1);
        big.changes[0].value = Some(vec![0; MAX_PAYLOAD_LEN + 1]);

        let refused = log.append(&big);
        let next = log.append(&record(2));

        assert!(matches!(refused, Err(LogError::RecordTooLarge { .. })));
        assert!(next.is_ok(), "{next:?}");
    }
}
