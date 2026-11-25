//! The append-only log: the one source of truth.
//!
//! A write is acknowledged only after its record is synced to storage. The
//! tree and the history are rebuilt from this log, so if the log is wrong,
//! everything above it is wrong too.

mod copy;
mod header;
pub(crate) mod record;
mod storage;

use std::fmt;
use std::io;

use header::{Header, Start};

pub(crate) use copy::CopyCheck;
pub(crate) use header::HEADER_LEN;
pub use header::LogId;
pub(crate) use header::is_start as is_start_of_a_log;
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
fn scan_after_header(bytes: &[u8], keep: bool) -> Result<record::Scan, LogError> {
    let scanned = if keep {
        record::scan(&bytes[HEADER_LEN..])
    } else {
        record::scan_frames(&bytes[HEADER_LEN..])
    };
    scanned.map_err(|error| match error {
        LogError::Corrupt { offset, reason } => LogError::Corrupt {
            offset: HEADER_LEN as u64 + offset,
            reason,
        },
        other => other,
    })
}

/// The records of a log that was just opened, decoded one at a time
/// from the bytes read then. See [`Log::open_lazily`].
pub(crate) struct Recorded {
    bytes: Vec<u8>,
    /// Where each record's frame ends in `bytes`.
    ends: Vec<u64>,
    next: usize,
}

impl Iterator for Recorded {
    /// A record, or that the frame written whole in its place does not
    /// decode: damage, as [`Log::open`] reports it.
    type Item = Result<Record, LogError>;

    fn next(&mut self) -> Option<Self::Item> {
        let end = *self.ends.get(self.next)? as usize;
        let start = match self.next.checked_sub(1) {
            Some(before) => self.ends[before] as usize,
            None => HEADER_LEN,
        };
        self.next += 1;
        Some(
            record::decode_frame(&self.bytes[start..end]).ok_or(LogError::Corrupt {
                offset: start as u64,
                reason: "checksummed payload does not decode",
            }),
        )
    }
}

/// A copy of a log's file from its start, checked all at once: what it
/// is, how many records it holds and the chain over them. Kept for the
/// tests of [`CopyCheck`], which reads the copy in parts and must agree.
#[cfg(test)]
pub(crate) fn check_copy_at_once(bytes: &[u8]) -> Result<(LogId, u64, Chain), LogError> {
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
    let scan = scan_after_header(bytes, true)?;
    let frames = &bytes[HEADER_LEN..];
    if scan.valid_len != frames.len() as u64 {
        return Err(LogError::Corrupt {
            offset: HEADER_LEN as u64 + scan.valid_len,
            reason: "a record cut short",
        });
    }
    let mut chain = header.chain_before;
    let mut start = 0;
    for &end in &scan.ends {
        chain = chained(&chain, &frames[start..end as usize]);
        start = end as usize;
    }
    Ok((header.id, scan.ends.len() as u64, chain))
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
    /// How many records the chain has been worked out over, and the
    /// chain over them: whoever asks again pays for what was written
    /// since, not for all that follows the last checkpoint.
    chained: (u64, Chain),
    /// The last answer of [`Log::chain_at`], and for how many records.
    answered: (u64, Chain),
    is_poisoned: bool,
    /// Set from opening until [`Log::settle`]: whether the file ends in
    /// a torn tail still to be cut.
    unsettled: Option<bool>,
}

impl<S: Storage> Log<S> {
    /// Open the log and return every acknowledged record in order. A torn
    /// tail left by an interrupted write is cut off before anything new is
    /// appended.
    pub fn open(storage: S) -> Result<(Self, Vec<Record>), LogError> {
        let (log, _, scan) = Self::open_keeping(storage, true)?;
        Ok((log, scan.records))
    }

    /// As [`Self::open`], with the records handed out one at a time as
    /// they are asked for, in order, so that no more than one is decoded
    /// at once: all of a log's records decoded together take several
    /// times the bytes of the log.
    ///
    /// Every frame's length and checksum are checked before this returns.
    /// A payload is decoded when its record is asked for, so a frame
    /// written whole that does not decode is reported then. Until every
    /// record has been taken and [`Self::settle`] called, the file is as
    /// it was found: a log that turns out to be damaged is not cut.
    pub(crate) fn open_lazily(storage: S) -> Result<(Self, Recorded), LogError> {
        let (log, bytes, _) = Self::open_keeping(storage, false)?;
        let recorded = Recorded {
            bytes,
            ends: log.ends.clone(),
            next: 0,
        };
        Ok((log, recorded))
    }

    fn open_keeping(mut storage: S, keep: bool) -> Result<(Self, Vec<u8>, record::Scan), LogError> {
        let bytes = storage.read_all()?;
        let (header, mut scan) = match header::read(&bytes) {
            Start::Header(header) => (header, scan_after_header(&bytes, keep)?),
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
        let has_torn_tail = valid_len < bytes.len() as u64;
        let mut log = Self {
            storage,
            header,
            len: valid_len,
            ends: scan
                .ends
                .iter()
                .map(|end| HEADER_LEN as u64 + end)
                .collect(),
            checkpoints: vec![header.chain_before],
            chained: (0, header.chain_before),
            answered: (0, header.chain_before),
            is_poisoned: false,
            unsettled: Some(has_torn_tail),
        };
        scan.ends = Vec::new();
        // Read a record at a time, the log is settled by whoever reads
        // it, once every record has decoded.
        if keep {
            log.settle()?;
        }
        Ok((log, bytes, scan))
    }

    /// Cuts a torn tail off the file, if opening found one, and syncs.
    /// Always the sync: a process that died between write and sync leaves
    /// records the kernel still serves but the disk may not hold, and
    /// handing them out before they are durable would acknowledge them
    /// twice over. Done once; nothing is appended before it.
    pub(crate) fn settle(&mut self) -> Result<(), LogError> {
        let Some(has_torn_tail) = self.unsettled.take() else {
            return Ok(());
        };
        if has_torn_tail {
            self.storage.truncate(self.len)?;
        }
        self.storage.sync()?;
        Ok(())
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

    /// Whether a write failed, after which the log takes no more.
    pub fn is_poisoned(&self) -> bool {
        self.is_poisoned
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

    /// Appends frames another log wrote, whole and already checked, and
    /// returns once they are durable: one sync for all of them. `ends`
    /// says where each ends in `frames`. For a replica, which takes in
    /// what its primary sends many records at a time.
    pub(crate) fn append_frames(&mut self, frames: &[u8], ends: &[u64]) -> Result<(), LogError> {
        if self.is_poisoned {
            return Err(LogError::Poisoned);
        }
        if let Err(error) = self.write_durably(frames) {
            self.is_poisoned = true;
            return Err(error.into());
        }
        let before = self.len;
        self.ends.extend(ends.iter().map(|end| before + end));
        self.len += frames.len() as u64;
        Ok(())
    }

    fn write_durably(&mut self, frame: &[u8]) -> io::Result<()> {
        // Never after a torn tail that was not cut.
        if let Some(has_torn_tail) = self.unsettled.take() {
            if has_torn_tail {
                self.storage.truncate(self.len)?;
            }
            self.storage.sync()?;
        }
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
        if records >= self.chained.0 {
            self.chain_up_to(records)?;
            self.answered = self.chained;
            return Ok(Some(self.chained.1));
        }
        // From the checkpoint before, or from the last answer if that
        // is nearer: a backup asks for the chain where the backup
        // before it ended, which is what it was last told.
        let kept = (records / CHAIN_EVERY) as usize;
        let checkpoint = (kept as u64 * CHAIN_EVERY, self.checkpoints[kept]);
        let (from, chain) = if (checkpoint.0..=records).contains(&self.answered.0) {
            self.answered
        } else {
            checkpoint
        };
        self.answered = (records, self.chain_over(chain, from, records)?);
        Ok(Some(self.answered.1))
    }

    /// Works out the chain over the next records it has not been worked
    /// out over, `max_len` bytes of them or one record if that is more,
    /// and returns whether any are left. [`Log::chain_at`] works out all
    /// that is missing at once, which for a log written to for a while
    /// is long; whoever cannot stop the writers that long calls this
    /// first until it says no.
    pub(crate) fn chain_step(&mut self, max_len: u64) -> io::Result<bool> {
        let done = self.chained.0 as usize;
        if done == self.ends.len() {
            return Ok(false);
        }
        let start = done
            .checked_sub(1)
            .map_or(HEADER_LEN as u64, |n| self.ends[n]);
        let within = self.ends[done..].partition_point(|&end| end - start <= max_len);
        self.chain_up_to((done + within.max(1)) as u64)?;
        Ok(self.chained.0 < self.records())
    }

    /// Carries the chain worked out so far on to the first `until`
    /// records, keeping it after each `CHAIN_EVERY` on the way.
    fn chain_up_to(&mut self, until: u64) -> io::Result<()> {
        while self.chained.0 < until {
            let (done, chain) = self.chained;
            let next = ((done / CHAIN_EVERY + 1) * CHAIN_EVERY).min(until);
            self.chained = (next, self.chain_over(chain, done, next)?);
            if next.is_multiple_of(CHAIN_EVERY) {
                self.checkpoints.push(self.chained.1);
            }
        }
        Ok(())
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

    /// How many bytes the frames of the records after the first `from`
    /// and up to the first `to` take, or `None` if the log does not hold
    /// `to` records or `to` is not after `from`.
    pub fn span(&self, from: u64, to: u64) -> Option<u64> {
        let (from, to) = (usize::try_from(from).ok()?, usize::try_from(to).ok()?);
        if to <= from || to > self.ends.len() {
            return None;
        }
        let start = from
            .checked_sub(1)
            .map_or(HEADER_LEN as u64, |before| self.ends[before]);
        Some(self.ends[to - 1] - start)
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// What the log's file starts with.
    pub(crate) fn header_bytes(&self) -> [u8; HEADER_LEN] {
        self.header.encode()
    }

    /// Where the acknowledged records end in the file, its header
    /// counted: a copy of the file up to here is a log that holds them
    /// all and nothing else.
    pub(crate) fn file_len(&self) -> u64 {
        self.len
    }

    /// `len` bytes of the file from `offset` on, read through the
    /// storage's own handle as the chain is.
    pub(crate) fn read_file(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.storage.read_at(offset, len)
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
    fn the_chain_worked_out_a_part_at_a_time_is_the_chain_worked_out_at_once() {
        let (mut at_once, _) = Log::open(SimStorage::default()).expect("opens");
        let (mut in_parts, _) = Log::open(SimStorage::default()).expect("opens");
        let records = CHAIN_EVERY as u32 + 40;
        for n in 1..=records {
            at_once.append(&record(n)).expect("appends");
            in_parts.append(&record(n)).expect("appends");
        }
        let frame = in_parts.len() / u64::from(records);

        let mut steps = 1;
        while in_parts.chain_step(frame * 100).expect("reads") {
            steps += 1;
        }

        assert_eq!(steps, records.div_ceil(100));
        // Each log is asked from the end back, so that the one asked at
        // once works out all of it in its first answer.
        for records in [CHAIN_EVERY + 40, CHAIN_EVERY, CHAIN_EVERY - 1, 100, 1, 0] {
            let chain = at_once.chain_at(records).expect("reads");
            assert!(chain.is_some());
            assert_eq!(in_parts.chain_at(records).expect("reads"), chain);
        }
        // Asked again where it was last asked, and a little after.
        for records in [100, 100, 130, 101, CHAIN_EVERY + 3, CHAIN_EVERY + 1] {
            assert_eq!(
                in_parts.chain_at(records).expect("reads"),
                Log::open(at_once.storage.clone())
                    .expect("opens")
                    .0
                    .chain_at(records)
                    .expect("reads"),
                "{records}"
            );
        }
        // Past what the steps covered, the chain goes on from there.
        at_once.append(&record(7)).expect("appends");
        in_parts.append(&record(7)).expect("appends");
        let last = CHAIN_EVERY + 41;
        assert!(!in_parts.chain_step(1).expect("reads"));
        assert_eq!(
            in_parts.chain_at(last).expect("reads"),
            at_once.chain_at(last).expect("reads")
        );
    }

    #[test]
    fn a_step_takes_one_record_however_little_it_may_read() {
        let (mut log, _) = Log::open(SimStorage::default()).expect("opens");
        for n in 1..=3 {
            log.append(&record(n)).expect("appends");
        }

        let left: Vec<bool> = (0..4).map(|_| log.chain_step(1).expect("reads")).collect();

        assert_eq!(left, [true, true, false, false]);
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
