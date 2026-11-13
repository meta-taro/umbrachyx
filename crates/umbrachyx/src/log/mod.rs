//! The append-only log: the one source of truth.
//!
//! A write is acknowledged only after its record is synced to storage. The
//! tree and the history are rebuilt from this log, so if the log is wrong,
//! everything above it is wrong too.

mod header;
pub(crate) mod record;
mod storage;

use std::fmt;
use std::io;

use header::{Header, Start};

pub(crate) use header::HEADER_LEN;
pub use header::LogId;
pub use record::{Branch, Commit, Edit, Engine, Erase, MAX_PAYLOAD_LEN, Record, Reject, Table};
pub(crate) use storage::sync_parent_dir;
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
    /// The file says it is a log in a layout this version does not know:
    /// one written by a later version.
    NewerFormat {
        version: u32,
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
            Self::NewerFormat { version } => write!(
                f,
                "log is in format {version}, written by a later version than this one"
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

/// A hash of every record up to some point in a log, in order. Two logs
/// with the same chain after as many records hold the same records.
pub type Chain = [u8; 32];

/// The chain is kept for every this many records; the chain at any other
/// record is worked out from the one kept before it.
const CHAIN_EVERY: u64 = 1024;

/// The chain of a log with no records.
fn chain_start() -> Chain {
    *blake3::hash(b"umbrachyx log").as_bytes()
}

/// The chain once `frame` follows the records `chain` covers.
fn chained(chain: &Chain, frame: &[u8]) -> Chain {
    let mut hasher = blake3::Hasher::new();
    hasher.update(chain);
    hasher.update(frame);
    *hasher.finalize().as_bytes()
}

/// The records after the header, with damage reported where it is in the
/// file.
fn scan_after_header(bytes: &[u8]) -> Result<record::Scan, LogError> {
    record::scan(&bytes[HEADER_LEN..]).map_err(|error| match error {
        LogError::Corrupt { offset, reason } => LogError::Corrupt {
            offset: HEADER_LEN as u64 + offset,
            reason,
        },
        other => other,
    })
}

#[derive(Debug)]
pub struct Log<S: Storage> {
    storage: S,
    /// What the file starts with: which log this is.
    header: Header,
    /// Where the acknowledged records end in the file, the header counted.
    len: u64,
    /// Where each acknowledged record's frame ends, in order: a record is
    /// named by its number, and this says where its bytes are.
    ends: Vec<u64>,
    /// The chain after each `CHAIN_EVERY` records, from none on, as far
    /// as anyone has asked: a log nobody replicates never works it out,
    /// so writing pays nothing for it.
    checkpoints: Vec<Chain>,
    is_poisoned: bool,
}

impl<S: Storage> Log<S> {
    /// Open the log and return every acknowledged record in order. A torn
    /// tail left by an interrupted write is cut off before anything new is
    /// appended.
    pub fn open(mut storage: S) -> Result<(Self, Vec<Record>), LogError> {
        let bytes = storage.read_all()?;
        let (header, scan) = match header::read(&bytes) {
            Start::Header(header) => (header, scan_after_header(&bytes)?),
            Start::Newer(version) => return Err(LogError::NewerFormat { version }),
            Start::None => (Self::start(&mut storage, &bytes)?, record::Scan::default()),
        };
        if header.first_record != 0 {
            return Err(LogError::Corrupt {
                offset: 0,
                reason: "the file is not the first of its log",
            });
        }
        let valid_len = HEADER_LEN as u64 + scan.valid_len;
        if valid_len < bytes.len() as u64 {
            storage.truncate(valid_len)?;
        }
        // Always sync: a process that died between write and sync leaves
        // records the kernel still serves but the disk may not hold. Handing
        // them out before they are durable would acknowledge them twice over.
        storage.sync()?;
        let log = Self {
            storage,
            header,
            len: valid_len,
            ends: scan
                .ends
                .iter()
                .map(|end| HEADER_LEN as u64 + end)
                .collect(),
            checkpoints: vec![header.chain_before],
            is_poisoned: false,
        };
        Ok((log, scan.records))
    }

    /// Makes a file with no header the start of a new log. Only a file
    /// that holds no record may be: nothing, zeros reserved ahead, or a
    /// header a crash cut short. With a whole record anywhere in it, the
    /// header in front of acknowledged records was damaged, or the file is
    /// from before logs had one, and it is refused rather than started
    /// over.
    fn start(storage: &mut S, bytes: &[u8]) -> Result<Header, LogError> {
        if record::holds_a_whole_frame(bytes) {
            return Err(LogError::Corrupt {
                offset: 0,
                reason: "records without a log header in front of them",
            });
        }
        let (id, created) = storage.origin()?;
        let header = Header {
            first_record: 0,
            chain_before: chain_start(),
            id,
            created,
        };
        storage.truncate(0)?;
        storage.append(&header.encode())?;
        Ok(header)
    }

    /// The name this log was given when it was made. Every copy of the
    /// log keeps it.
    pub fn id(&self) -> LogId {
        self.header.id
    }

    /// When the log was made, in microseconds since the Unix epoch.
    pub fn created(&self) -> u64 {
        self.header.created
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
        self.ends.push(self.len);
        Ok(())
    }

    fn write_durably(&mut self, frame: &[u8]) -> io::Result<()> {
        self.storage.append(frame)?;
        self.storage.sync()
    }

    /// Bytes of acknowledged records in the log, its header not counted.
    pub fn len(&self) -> u64 {
        self.len - HEADER_LEN as u64
    }

    /// How many records are acknowledged. A record is named by how many
    /// come before it, which stays true however the log is later laid
    /// out in files.
    pub fn records(&self) -> u64 {
        self.ends.len() as u64
    }

    /// The chain over the first `records` records, or `None` if the log
    /// holds fewer. A replica that holds as many says what its own chain
    /// is; if the two differ, it does not hold this log's records and
    /// must not be sent what follows them.
    pub fn chain_at(&mut self, records: u64) -> io::Result<Option<Chain>> {
        if records > self.records() {
            return Ok(None);
        }
        let kept = (records / CHAIN_EVERY) as usize;
        while self.checkpoints.len() <= kept {
            let from = (self.checkpoints.len() as u64 - 1) * CHAIN_EVERY;
            let last = self.checkpoints[self.checkpoints.len() - 1];
            let next = self.chain_over(last, from, from + CHAIN_EVERY)?;
            self.checkpoints.push(next);
        }
        let from = kept as u64 * CHAIN_EVERY;
        self.chain_over(self.checkpoints[kept], from, records)
            .map(Some)
    }

    /// `chain`, which covers the first `from` records, carried on over
    /// the records up to the first `until`.
    fn chain_over(&mut self, mut chain: Chain, from: u64, until: u64) -> io::Result<Chain> {
        let (from, until) = (from as usize, until as usize);
        if until <= from {
            return Ok(chain);
        }
        let first = from
            .checked_sub(1)
            .map_or(HEADER_LEN as u64, |before| self.ends[before]);
        let last = self.ends[until - 1];
        let bytes = self.storage.read_at(first, (last - first) as usize)?;
        let mut start = first;
        for &end in &self.ends[from..until] {
            chain = chained(
                &chain,
                &bytes[(start - first) as usize..(end - first) as usize],
            );
            start = end;
        }
        Ok(chain)
    }

    /// The frames of the records after the first `records`, as the log
    /// holds them: whole frames only, as many as fit in `max_bytes` and
    /// at least one if there is one. Only acknowledged records are ever
    /// here: a record still being written, or one whose sync failed, has
    /// no number yet.
    pub fn read_after(&mut self, records: u64, max_bytes: usize) -> io::Result<Vec<u8>> {
        let Ok(first) = usize::try_from(records) else {
            return Ok(Vec::new());
        };
        if first >= self.ends.len() {
            return Ok(Vec::new());
        }
        let start = first
            .checked_sub(1)
            .map_or(HEADER_LEN as u64, |before| self.ends[before]);
        let limit = start.saturating_add(max_bytes as u64);
        let whole = self.ends[first..].partition_point(|&end| end <= limit);
        let end = self.ends[first + whole.max(1) - 1];
        self.storage.read_at(start, (end - start) as usize)
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
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
        Record::Commit(Commit {
            branch: "main".to_owned(),
            changes: vec![Edit {
                key: format!("player:{n}:coins").into_bytes(),
                value: Some(n.to_le_bytes().to_vec()),
            }],
            who: "agent:shop".to_owned(),
            why: "purchase".to_owned(),
            approved_by: None,
            time: u64::from(n),
            merged: None,
        })
    }

    fn written(records: &[Record]) -> SimStorage {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for r in records {
            log.append(r).expect("appends");
        }
        log.storage
    }

    fn frames(records: &[Record]) -> Vec<u8> {
        records
            .iter()
            .flat_map(|r| record::encode(r).expect("encodes"))
            .collect()
    }

    #[test]
    fn records_are_read_back_by_number_as_the_bytes_the_log_holds() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for n in 1..=3 {
            log.append(&record(n)).expect("appends");
        }

        assert_eq!(log.records(), 3);
        assert_eq!(
            log.read_after(0, usize::MAX).expect("reads"),
            frames(&[record(1), record(2), record(3)])
        );
        assert_eq!(
            log.read_after(1, usize::MAX).expect("reads"),
            frames(&[record(2), record(3)])
        );
        assert!(log.read_after(3, usize::MAX).expect("reads").is_empty());
        assert!(log.read_after(9, usize::MAX).expect("reads").is_empty());
    }

    #[test]
    fn a_read_stops_at_a_whole_record_and_gives_at_least_one() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for n in 1..=3 {
            log.append(&record(n)).expect("appends");
        }
        let one = frames(&[record(1)]).len();

        // Less than a record is asked for: it comes whole all the same.
        assert_eq!(log.read_after(0, 1).expect("reads"), frames(&[record(1)]));
        // Room for one and a half: only the whole one.
        assert_eq!(
            log.read_after(1, one + one / 2).expect("reads"),
            frames(&[record(2)])
        );
        assert_eq!(
            log.read_after(1, 2 * one).expect("reads"),
            frames(&[record(2), record(3)])
        );
    }

    #[test]
    fn a_record_whose_sync_failed_is_never_read_back() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        log.append(&record(1)).expect("appends");
        log.storage_mut().fail_next_sync();

        log.append(&record(2)).expect_err("the sync fails");

        assert_eq!(log.records(), 1);
        assert_eq!(
            log.read_after(0, usize::MAX).expect("reads"),
            frames(&[record(1)])
        );
    }

    #[test]
    fn records_keep_their_numbers_when_the_log_is_opened_again() {
        let storage = written(&[record(1), record(2), record(3)]);

        let (mut log, _) = Log::open(storage.crash(0)).expect("reopens");
        log.append(&record(4)).expect("appends");

        assert_eq!(log.records(), 4);
        assert_eq!(
            log.read_after(2, usize::MAX).expect("reads"),
            frames(&[record(3), record(4)])
        );
    }

    #[test]
    fn two_logs_with_the_same_records_have_the_same_chain_at_every_record() {
        let (mut one, _) = Log::open(SimStorage::default()).expect("opens");
        let (mut other, _) = Log::open(SimStorage::default()).expect("opens");
        for n in 1..=5 {
            one.append(&record(n)).expect("appends");
            other.append(&record(n)).expect("appends");
        }

        for records in 0..=5 {
            let chain = one.chain_at(records).expect("reads");
            assert!(chain.is_some());
            assert_eq!(chain, other.chain_at(records).expect("reads"));
        }
        assert_eq!(one.chain_at(6).expect("reads"), None, "past its end");
    }

    #[test]
    fn logs_that_differ_in_one_record_have_different_chains_from_there_on() {
        let (mut one, _) = Log::open(SimStorage::default()).expect("opens");
        let (mut other, _) = Log::open(SimStorage::default()).expect("opens");
        for n in 1..=4 {
            one.append(&record(n)).expect("appends");
            // The second record differs; the ones after it are the same.
            other
                .append(&record(if n == 2 { 20 } else { n }))
                .expect("appends");
        }

        assert_eq!(
            one.chain_at(1).expect("reads"),
            other.chain_at(1).expect("reads")
        );
        for records in 2..=4 {
            assert_ne!(
                one.chain_at(records).expect("reads"),
                other.chain_at(records).expect("reads"),
                "after {records} records"
            );
        }
    }

    #[test]
    fn the_chain_is_the_same_after_the_log_is_opened_again_and_past_a_checkpoint() {
        let records: Vec<Record> = (1..=(2 * CHAIN_EVERY as u32 + 3)).map(record).collect();
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for r in &records {
            log.append(r).expect("appends");
        }
        let probes = [
            0,
            1,
            CHAIN_EVERY - 1,
            CHAIN_EVERY,
            CHAIN_EVERY + 1,
            2 * CHAIN_EVERY + 3,
        ];
        let before: Vec<_> = probes
            .iter()
            .map(|&n| log.chain_at(n).expect("reads"))
            .collect();

        let (mut reopened, _) = Log::open(log.storage.crash(0)).expect("reopens");

        let after: Vec<_> = probes
            .iter()
            .map(|&n| reopened.chain_at(n).expect("reads"))
            .collect();
        assert_eq!(after, before);
        assert!(before.iter().all(Option::is_some));
        let unique: std::collections::HashSet<_> = before.iter().collect();
        assert_eq!(unique.len(), probes.len(), "every prefix has its own chain");
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
        bytes[HEADER_LEN + record::HEADER_LEN + 1] ^= 0x80;

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
        if let Record::Commit(commit) = &mut big {
            commit.changes[0].value = Some(vec![0; MAX_PAYLOAD_LEN + 1]);
        }

        let refused = log.append(&big);
        let next = log.append(&record(2));

        assert!(matches!(refused, Err(LogError::RecordTooLarge { .. })));
        assert!(next.is_ok(), "{next:?}");
    }
    /// A header as `open` writes one on a simulated disk.
    fn header_bytes() -> Vec<u8> {
        let (_, created) = SimStorage::default().origin().expect("has an origin");
        Header {
            first_record: 0,
            chain_before: chain_start(),
            id: *b"a simulated log.",
            created,
        }
        .encode()
        .to_vec()
    }

    #[test]
    fn a_new_log_starts_with_a_header_and_keeps_its_name_when_opened_again() {
        let (log, records) = Log::open(SimStorage::default()).expect("opens");
        let id = log.id();
        let mut storage = log.storage;

        assert!(records.is_empty());
        assert_eq!(storage.read_all().expect("reads"), header_bytes());
        let (reopened, _) = Log::open(storage.crash(0)).expect("reopens");
        assert_eq!(reopened.id(), id);
        assert!(reopened.is_empty());
        assert_eq!(reopened.len(), 0);
    }

    #[test]
    fn the_header_is_durable_before_open_returns() {
        let (log, _) = Log::open(SimStorage::default()).expect("opens");

        assert_eq!(log.storage.durable_len(), HEADER_LEN);
    }

    #[test]
    fn records_follow_the_header_as_the_frames_they_are() {
        let records = [record(1), record(2)];

        let mut storage = written(&records);

        let expected = [header_bytes(), frames(&records)].concat();
        assert_eq!(storage.read_all().expect("reads"), expected);
    }

    #[test]
    fn records_with_no_header_in_front_are_refused_not_started_over() {
        let bytes = frames(&[record(1), record(2)]);

        let error = Log::open(SimStorage::with_bytes(bytes)).expect_err("has no header");

        assert!(
            matches!(error, LogError::Corrupt { offset: 0, reason } if reason.contains("header")),
            "{error}"
        );
    }

    #[test]
    fn a_header_cut_short_with_nothing_after_it_is_written_again() {
        for cut in 0..HEADER_LEN {
            let torn = header_bytes()[..cut].to_vec();

            let (log, records) = Log::open(SimStorage::with_bytes(torn)).expect("opens");

            assert!(records.is_empty(), "cut at {cut}");
            let mut storage = log.storage;
            assert_eq!(storage.read_all().expect("reads"), header_bytes());
        }
    }

    #[test]
    fn zeros_reserved_ahead_with_no_header_are_a_new_log() {
        let (mut log, records) = Log::open(SimStorage::reserving(1_024)).expect("opens");
        log.append(&record(1)).expect("appends");

        assert!(records.is_empty());
        let (_, recovered) = Log::open(log.storage.crash(0)).expect("reopens");
        assert_eq!(recovered, vec![record(1)]);
    }

    #[test]
    fn a_damaged_header_in_front_of_records_refuses_to_open() {
        let whole = written(&[record(1), record(2)]).read_all().expect("reads");

        for bit in 0..HEADER_LEN * 8 {
            let mut bytes = whole.clone();
            bytes[bit / 8] ^= 1 << (bit % 8);

            let error = Log::open(SimStorage::with_bytes(bytes)).expect_err("header is damaged");

            assert!(
                matches!(
                    error,
                    LogError::Corrupt { offset: 0, .. } | LogError::NewerFormat { .. }
                ),
                "bit {bit}: {error}"
            );
        }
    }

    #[test]
    fn a_log_of_a_later_format_is_refused_as_such() {
        let mut bytes = header_bytes();
        bytes[8..12].copy_from_slice(&2_u32.to_le_bytes());

        let error = Log::open(SimStorage::with_bytes(bytes)).expect_err("is newer");

        assert!(
            matches!(error, LogError::NewerFormat { version: 2 }),
            "{error}"
        );
        assert!(error.to_string().contains("later version"));
    }

    #[test]
    fn a_file_that_is_not_the_first_of_its_log_is_refused() {
        let later = Header {
            first_record: 5,
            chain_before: chain_start(),
            id: [1; 16],
            created: 0,
        };

        let error =
            Log::open(SimStorage::with_bytes(later.encode().to_vec())).expect_err("not first");

        assert!(matches!(error, LogError::Corrupt { offset: 0, .. }));
    }

    #[test]
    fn damage_in_a_record_is_reported_where_it_is_in_the_file() {
        let records = [record(1), record(2), record(3)];
        let mut bytes = written(&records).read_all().expect("reads");
        let second = HEADER_LEN + frames(&records[..1]).len();
        bytes[second + 10] ^= 1;

        let error = Log::open(SimStorage::with_bytes(bytes)).expect_err("is damaged");

        assert!(
            matches!(error, LogError::Corrupt { offset, .. } if offset == second as u64),
            "{error}"
        );
    }
}
