//! The on-disk form of one log record.
//!
//! A frame is `len: u32 LE | crc32: u32 LE | payload`, where the checksum
//! covers the payload only. The payload starts with a format version, then
//! the record's fields, each one length-prefixed, so a reader never has to
//! guess where a field ends.

use super::LogError;

/// One entry of the log. A record is one frame, so it is either wholly in
/// the log or not at all.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Record {
    Commit(Commit),
    Branch(Branch),
    /// Commits acknowledged together by one sync, in the order they apply.
    /// One frame, so a crash keeps all of them or none: the log has at most
    /// one frame in flight, which is what tells a torn tail from damage.
    Group(Vec<Commit>),
    /// A subject's key was destroyed. Holds no value.
    Erase(Erase),
    /// A table declared to be held by an engine.
    Table(Table),
    /// A change waiting on a branch, closed without merging.
    Reject(Reject),
}

/// Changes to one branch, made together, with who made them, why, who
/// approved them and when. However many keys it touches, it is one record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Commit {
    pub branch: String,
    pub changes: Vec<Edit>,
    pub who: String,
    pub why: String,
    pub approved_by: Option<String>,
    /// Microseconds since the Unix epoch, from the store's clock.
    pub time: u64,
    /// For a merge, the head of the branch merged in: the commit's second
    /// parent. The first is the branch's previous head.
    pub merged: Option<[u8; 32]>,
}

/// A subject erased: which one, which of its keys, who did it, why, who
/// approved it and when. Written before the key is destroyed, so a crash in
/// between is finished when the log is next opened.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Erase {
    pub subject: Vec<u8>,
    pub key_fingerprint: [u8; 8],
    pub who: String,
    pub why: String,
    pub approved_by: Option<String>,
    /// Microseconds since the Unix epoch, from the store's clock.
    pub time: u64,
}

/// How a table is held in memory. The log holds every write either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Engine {
    /// Every change, in the ordered tree: today's way, and a table's way
    /// until it is declared otherwise.
    History,
    /// Only the current value of each key.
    Latest,
}

impl Engine {
    /// The name tools see and the command line takes.
    pub const fn name(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Latest => "latest",
        }
    }

    /// The engine called `name`, as [`Engine::name`] spells it.
    pub fn named(name: &str) -> Option<Self> {
        [Self::History, Self::Latest]
            .into_iter()
            .find(|engine| engine.name() == name)
    }

    const fn byte(self) -> u8 {
        match self {
            Self::History => 0,
            Self::Latest => 1,
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::History),
            1 => Some(Self::Latest),
            _ => None,
        }
    }
}

/// A table, the first part of its keys, declared to be held by `engine`,
/// with who declared it, why, who approved it and when.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Table {
    pub name: Vec<u8>,
    pub engine: Engine,
    pub who: String,
    pub why: String,
    pub approved_by: Option<String>,
    /// Microseconds since the Unix epoch, from the store's clock.
    pub time: u64,
}

/// A branch's change, as it stood at `head`, rejected: who, why and when.
/// Commits written to the branch after it make a new change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reject {
    pub branch: String,
    pub head: [u8; 32],
    pub who: String,
    pub why: String,
    /// Microseconds since the Unix epoch, from the store's clock.
    pub time: u64,
}

/// A new branch, starting where `from` is now.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Branch {
    pub name: String,
    pub from: String,
}

/// One key set to a value, or removed when `value` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Edit {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// Bytes in front of every payload: its length and its checksum.
pub const HEADER_LEN: usize = 8;

/// The largest payload a frame may carry. A length beyond this is read as
/// damage, not as a request to allocate gigabytes.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024 * 1024;

/// What survives a scan of the log bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Scan {
    pub records: Vec<Record>,
    /// Length of the prefix made of whole, checked records. Anything after it
    /// is a torn tail from an interrupted write and is safe to cut off.
    pub valid_len: u64,
}

const LEN_PREFIX: usize = 4;
/// Bumped whenever the payload layout changes. A frame with another version
/// cannot be read and is reported as damage, never skipped.
const FORMAT_VERSION: u8 = 3;
const KIND_COMMIT: u8 = 1;
const KIND_BRANCH: u8 = 2;
const KIND_GROUP: u8 = 3;
const KIND_ERASE: u8 = 4;
const KIND_TABLE: u8 = 5;
const KIND_REJECT: u8 = 6;
/// The version and kind bytes that start every payload.
const KIND_PREFIX: usize = 2;
/// The bytes in front of a group's commits: the version, kind and count.
pub const GROUP_OVERHEAD: usize = KIND_PREFIX + LEN_PREFIX;
const TIME_LEN: usize = 8;
const ID_LEN: usize = 32;
const ABSENT: u8 = 0;
const PRESENT: u8 = 1;

pub fn encode(record: &Record) -> Result<Vec<u8>, LogError> {
    let payload = encode_payload(record)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| LogError::RecordTooLarge { len: payload.len() })?;
    let crc = crc32fast::hash(&payload);
    Ok([&len.to_le_bytes()[..], &crc.to_le_bytes(), &payload].concat())
}

fn encode_payload(record: &Record) -> Result<Vec<u8>, LogError> {
    let payload = match record {
        Record::Commit(commit) => {
            let mut payload = vec![FORMAT_VERSION, KIND_COMMIT];
            push_commit(&mut payload, commit)?;
            payload
        }
        Record::Branch(branch) => {
            let mut payload = vec![FORMAT_VERSION, KIND_BRANCH];
            push_field(&mut payload, branch.name.as_bytes());
            push_field(&mut payload, branch.from.as_bytes());
            payload
        }
        Record::Group(commits) => {
            let mut payload = vec![FORMAT_VERSION, KIND_GROUP];
            push_count(&mut payload, commits.len())?;
            for commit in commits {
                push_commit(&mut payload, commit)?;
            }
            payload
        }
        Record::Erase(erase) => {
            let mut payload = vec![FORMAT_VERSION, KIND_ERASE];
            push_field(&mut payload, &erase.subject);
            push_field(&mut payload, &erase.key_fingerprint);
            push_field(&mut payload, erase.who.as_bytes());
            push_field(&mut payload, erase.why.as_bytes());
            push_optional(
                &mut payload,
                erase.approved_by.as_deref().map(str::as_bytes),
            );
            payload.extend_from_slice(&erase.time.to_le_bytes());
            payload
        }
        Record::Table(table) => {
            let mut payload = vec![FORMAT_VERSION, KIND_TABLE];
            push_field(&mut payload, &table.name);
            payload.push(table.engine.byte());
            push_field(&mut payload, table.who.as_bytes());
            push_field(&mut payload, table.why.as_bytes());
            push_optional(
                &mut payload,
                table.approved_by.as_deref().map(str::as_bytes),
            );
            payload.extend_from_slice(&table.time.to_le_bytes());
            payload
        }
        Record::Reject(reject) => {
            let mut payload = vec![FORMAT_VERSION, KIND_REJECT];
            push_field(&mut payload, reject.branch.as_bytes());
            push_field(&mut payload, &reject.head);
            push_field(&mut payload, reject.who.as_bytes());
            push_field(&mut payload, reject.why.as_bytes());
            payload.extend_from_slice(&reject.time.to_le_bytes());
            payload
        }
    };
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(LogError::RecordTooLarge { len: payload.len() });
    }
    Ok(payload)
}

/// What a commit adds to a payload, in bytes: exactly what `push_commit`
/// writes for it. A commit record's payload is this plus the version and
/// kind; a group's is the sum over its commits plus `GROUP_OVERHEAD`.
pub fn commit_len(
    branch: &str,
    who: &str,
    why: &str,
    approved_by: Option<&str>,
    is_merge: bool,
    changes: &[Edit],
) -> usize {
    let field = |len: usize| LEN_PREFIX + len;
    let optional = |len: Option<usize>| 1 + len.map_or(0, field);
    let edits: usize = changes
        .iter()
        .map(|edit| field(edit.key.len()) + optional(edit.value.as_ref().map(Vec::len)))
        .sum();
    field(branch.len())
        + field(who.len())
        + field(why.len())
        + optional(approved_by.map(str::len))
        + TIME_LEN
        + optional(is_merge.then_some(ID_LEN))
        + LEN_PREFIX
        + edits
}

fn push_count(payload: &mut Vec<u8>, count: usize) -> Result<(), LogError> {
    let count = u32::try_from(count).map_err(|_| LogError::RecordTooLarge { len: count })?;
    payload.extend_from_slice(&count.to_le_bytes());
    Ok(())
}

fn push_commit(payload: &mut Vec<u8>, record: &Commit) -> Result<(), LogError> {
    push_field(payload, record.branch.as_bytes());
    push_field(payload, record.who.as_bytes());
    push_field(payload, record.why.as_bytes());
    push_optional(payload, record.approved_by.as_deref().map(str::as_bytes));
    payload.extend_from_slice(&record.time.to_le_bytes());
    push_optional(payload, record.merged.as_ref().map(|id| &id[..]));
    push_count(payload, record.changes.len())?;
    for write in &record.changes {
        push_field(payload, &write.key);
        push_optional(payload, write.value.as_deref());
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(LogError::RecordTooLarge { len: payload.len() });
        }
    }
    Ok(())
}

fn push_optional(payload: &mut Vec<u8>, field: Option<&[u8]>) {
    match field {
        None => payload.push(ABSENT),
        Some(field) => {
            payload.push(PRESENT);
            push_field(payload, field);
        }
    }
}

fn push_field(payload: &mut Vec<u8>, field: &[u8]) {
    // A field longer than u32 cannot fit in MAX_PAYLOAD_LEN either; it is
    // cut here and the payload is refused for its size right after.
    let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
    payload.extend_from_slice(&len.to_le_bytes());
    payload.extend_from_slice(field);
}

/// Read every whole record from the start of `bytes`.
///
/// Only the end of the log may be damaged by a crash, because appends are
/// synchronous and at most one is in flight. So when a frame does not check
/// out, the bytes after it decide: if a whole, checked frame starts anywhere
/// in them, acknowledged data was damaged and the scan fails rather than
/// dropping it. Otherwise the rest is a torn tail.
///
/// One case cannot be told apart: damage inside the last acknowledged frame
/// looks exactly like a torn write of it.
pub fn scan(bytes: &[u8]) -> Result<Scan, LogError> {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match read_frame(&bytes[offset..], offset as u64)? {
            Frame::Whole { record, len } => {
                records.push(record);
                offset += len;
            }
            Frame::TornTail => break,
        }
    }
    Ok(Scan {
        records,
        valid_len: offset as u64,
    })
}

enum Frame {
    Whole { record: Record, len: usize },
    TornTail,
}

/// The smallest payload `encode` can produce: the version, the kind and a
/// branch record's two empty fields. Anything shorter is not a frame, which
/// also keeps a zeroed header (`len 0`, `crc32("") == 0`) from checking out.
const MIN_PAYLOAD_LEN: usize = KIND_PREFIX + 2 * LEN_PREFIX;

fn read_frame(rest: &[u8], offset: u64) -> Result<Frame, LogError> {
    match check_frame(rest) {
        Ok(Some(frame)) => Ok(frame),
        // The checksum held, so this is not a torn write: it was written
        // whole and cannot be read, by this version or at all.
        Ok(None) => Err(LogError::Corrupt {
            offset,
            reason: "checksummed payload does not decode",
        }),
        Err(reason) if has_whole_frame_after_start(rest) => {
            Err(LogError::Corrupt { offset, reason })
        }
        Err(_) => Ok(Frame::TornTail),
    }
}

/// The frame at the start of `rest`, or why there is none. `Ok(None)` is a
/// frame whose checksum holds but whose payload does not decode.
fn check_frame(rest: &[u8]) -> Result<Option<Frame>, &'static str> {
    if rest.len() < HEADER_LEN {
        return Err("header cut short");
    }
    let len = read_u32(rest, 0) as usize;
    if !(MIN_PAYLOAD_LEN..=MAX_PAYLOAD_LEN).contains(&len) {
        return Err("payload length out of range");
    }
    let end = HEADER_LEN + len;
    let payload = rest.get(HEADER_LEN..end).ok_or("payload cut short")?;
    if crc32fast::hash(payload) != read_u32(rest, LEN_PREFIX) {
        return Err("checksum mismatch");
    }
    Ok(decode_payload(payload).map(|record| Frame::Whole { record, len: end }))
}

/// Whether a whole, checked frame starts anywhere after the first byte.
///
/// Recovery-only. No frame starts in the zeros that end `rest`: a frame's
/// length is never zero. A log file reserved ahead of its records ends in
/// megabytes of them, so the search stops at the last byte that is not
/// zero. Before that, most offsets are rejected on the length field alone.
fn has_whole_frame_after_start(rest: &[u8]) -> bool {
    let Some(last) = rest.iter().rposition(|&byte| byte != 0) else {
        return false;
    };
    (1..=last).any(|start| check_frame(&rest[start..]).is_ok())
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn decode_payload(payload: &[u8]) -> Option<Record> {
    let mut cursor = Cursor { rest: payload };
    if cursor.byte()? != FORMAT_VERSION {
        return None;
    }
    let record = match cursor.byte()? {
        KIND_COMMIT => Record::Commit(decode_commit(&mut cursor)?),
        KIND_BRANCH => Record::Branch(Branch {
            name: cursor.text()?,
            from: cursor.text()?,
        }),
        KIND_GROUP => Record::Group(decode_group(&mut cursor)?),
        KIND_ERASE => Record::Erase(decode_erase(&mut cursor)?),
        KIND_TABLE => Record::Table(decode_table(&mut cursor)?),
        KIND_REJECT => Record::Reject(Reject {
            branch: cursor.text()?,
            head: <[u8; 32]>::try_from(cursor.field()?).ok()?,
            who: cursor.text()?,
            why: cursor.text()?,
            time: cursor.u64()?,
        }),
        _ => return None,
    };
    cursor.rest.is_empty().then_some(record)
}

fn decode_erase(cursor: &mut Cursor<'_>) -> Option<Erase> {
    let subject = cursor.field()?.to_vec();
    let key_fingerprint = <[u8; 8]>::try_from(cursor.field()?).ok()?;
    let who = cursor.text()?;
    let why = cursor.text()?;
    let approved_by = match cursor.optional()? {
        None => None,
        Some(bytes) => Some(bytes_to_text(bytes)?),
    };
    let time = cursor.u64()?;
    Some(Erase {
        subject,
        key_fingerprint,
        who,
        why,
        approved_by,
        time,
    })
}

fn decode_table(cursor: &mut Cursor<'_>) -> Option<Table> {
    let name = cursor.field()?.to_vec();
    let engine = Engine::from_byte(cursor.byte()?)?;
    let who = cursor.text()?;
    let why = cursor.text()?;
    let approved_by = match cursor.optional()? {
        None => None,
        Some(bytes) => Some(bytes_to_text(bytes)?),
    };
    let time = cursor.u64()?;
    Some(Table {
        name,
        engine,
        who,
        why,
        approved_by,
        time,
    })
}

fn decode_group(cursor: &mut Cursor<'_>) -> Option<Vec<Commit>> {
    let count = read_u32(cursor.take(LEN_PREFIX)?, 0);
    (0..count).map(|_| decode_commit(cursor)).collect()
}

fn decode_commit(cursor: &mut Cursor<'_>) -> Option<Commit> {
    let branch = cursor.text()?;
    let who = cursor.text()?;
    let why = cursor.text()?;
    let approved_by = match cursor.optional()? {
        None => None,
        Some(bytes) => Some(bytes_to_text(bytes)?),
    };
    let time = cursor.u64()?;
    let merged = match cursor.optional()? {
        None => None,
        Some(id) => Some(<[u8; 32]>::try_from(id).ok()?),
    };
    let count = read_u32(cursor.take(LEN_PREFIX)?, 0);
    let mut changes = Vec::new();
    for _ in 0..count {
        let key = cursor.field()?.to_vec();
        let value = cursor.optional()?.map(<[u8]>::to_vec);
        changes.push(Edit { key, value });
    }
    Some(Commit {
        branch,
        changes,
        who,
        why,
        approved_by,
        time,
        merged,
    })
}

fn bytes_to_text(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.rest.split_at_checked(n)?;
        self.rest = tail;
        Some(head)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn field(&mut self) -> Option<&'a [u8]> {
        let len = read_u32(self.take(LEN_PREFIX)?, 0) as usize;
        self.take(len)
    }

    fn text(&mut self) -> Option<String> {
        bytes_to_text(self.field()?)
    }

    fn u64(&mut self) -> Option<u64> {
        let mut word = [0; 8];
        word.copy_from_slice(self.take(8)?);
        Some(u64::from_le_bytes(word))
    }

    /// `None` when the bytes do not decode; `Some(None)` for an absent field.
    fn optional(&mut self) -> Option<Option<&'a [u8]>> {
        match self.byte()? {
            ABSENT => Some(None),
            PRESENT => self.field().map(Some),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, approved_by: Option<&str>) -> Record {
        Record::Commit(commit(key, approved_by))
    }

    fn commit(key: &str, approved_by: Option<&str>) -> Commit {
        Commit {
            branch: "main".to_owned(),
            changes: vec![
                Edit {
                    key: key.as_bytes().to_vec(),
                    value: Some(vec![0, 1, 2, 255]),
                },
                Edit {
                    key: b"removed".to_vec(),
                    value: None,
                },
            ],
            who: "agent:inventory".to_owned(),
            why: "grant login bonus — 初回".to_owned(),
            approved_by: approved_by.map(str::to_owned),
            time: 1_800_000_000_000_000,
            merged: None,
        }
    }

    fn frames(records: &[Record]) -> Vec<u8> {
        records
            .iter()
            .flat_map(|r| encode(r).expect("encodes"))
            .collect()
    }

    #[test]
    fn records_round_trip_with_and_without_approver() {
        let records = vec![record("a", Some("ops:lead")), record("b", None)];
        let bytes = frames(&records);

        let scan = scan(&bytes).expect("scans");

        assert_eq!(scan.records, records);
        assert_eq!(scan.valid_len, bytes.len() as u64);
    }

    #[test]
    fn empty_log_scans_to_nothing() {
        let scan = scan(&[]).expect("scans");

        assert!(scan.records.is_empty());
        assert_eq!(scan.valid_len, 0);
    }

    #[test]
    fn every_cut_inside_the_last_frame_is_a_torn_tail() {
        let first = frames(&[record("a", None)]);
        let both = frames(&[record("a", None), record("b", None)]);

        for cut in first.len() + 1..both.len() {
            let scan = scan(&both[..cut]).expect("a torn tail is not an error");

            assert_eq!(scan.records, vec![record("a", None)], "cut at {cut}");
            assert_eq!(scan.valid_len, first.len() as u64, "cut at {cut}");
        }
    }

    #[test]
    fn damaged_last_frame_is_a_torn_tail() {
        let first = frames(&[record("a", None)]);
        let mut bytes = frames(&[record("a", None), record("b", None)]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;

        let scan = scan(&bytes).expect("scans");

        assert_eq!(scan.records, vec![record("a", None)]);
        assert_eq!(scan.valid_len, first.len() as u64);
    }

    #[test]
    fn zero_filled_tail_is_a_torn_tail() {
        let first = frames(&[record("a", None)]);
        let bytes = [first.clone(), vec![0; 4096]].concat();

        let scan = scan(&bytes).expect("scans");

        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.valid_len, first.len() as u64);
    }

    #[test]
    fn damage_before_the_last_frame_is_corruption() {
        let mut bytes = frames(&[record("a", None), record("b", None)]);
        bytes[HEADER_LEN] ^= 0x01;

        let error = scan(&bytes).expect_err("acknowledged data was damaged");

        assert!(
            matches!(error, LogError::Corrupt { offset: 0, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn oversized_length_before_a_whole_frame_is_corruption() {
        let mut bytes = frames(&[record("a", None), record("b", None)]);
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());

        let error = scan(&bytes).expect_err("length cannot be trusted");

        assert!(
            matches!(error, LogError::Corrupt { offset: 0, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn interior_length_damaged_to_point_past_the_end_is_corruption() {
        let mut bytes = frames(&[record("a", None), record("b", None), record("c", None)]);
        let past_end = u32::try_from(bytes.len()).expect("small");
        bytes[..4].copy_from_slice(&past_end.to_le_bytes());

        let error = scan(&bytes).expect_err("later acknowledged frames would be dropped");

        assert!(
            matches!(error, LogError::Corrupt { offset: 0, .. }),
            "{error:?}"
        );
    }

    /// A log reserved ahead of its records: megabytes of zeros after the
    /// last record, or after a record cut short, are a tail to cut, found
    /// without trying a frame at every one of them.
    #[test]
    fn a_long_zero_tail_is_cut_wherever_the_records_end() {
        let records = vec![record("a", None), record("b", None)];
        let whole = frames(&records);
        let reserve = vec![0; 8 * 1024 * 1024];
        let started = std::time::Instant::now();

        for cut in [whole.len(), whole.len() - 20] {
            let bytes = [&whole[..cut], &reserve[..]].concat();
            let scan = scan(&bytes).expect("scans");

            let kept = if cut == whole.len() { 2 } else { 1 };
            assert_eq!(scan.records, records[..kept], "cut at {cut}");
            assert!(scan.valid_len <= cut as u64, "cut at {cut}");
        }
        // The last byte of this record is a zero. Cut off and followed by
        // the reserve's zeros, the bytes are those of the whole record, and
        // it reads as written: nothing tells the two apart, and nothing
        // needs to.
        assert_eq!(whole.last(), Some(&0));
        let bytes = [&whole[..whole.len() - 1], &reserve[..]].concat();
        assert_eq!(scan(&bytes).expect("scans").records, records);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "scanning the zeros took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn zero_sector_followed_by_garbage_is_a_torn_tail() {
        let first = frames(&[record("a", None)]);
        let bytes = [first.clone(), vec![0; 512], vec![0xAB; 512]].concat();

        let scan = scan(&bytes).expect("a torn multi-sector write is not an error");

        assert_eq!(scan.records, vec![record("a", None)]);
        assert_eq!(scan.valid_len, first.len() as u64);
    }

    #[test]
    fn oversized_record_is_refused_before_writing() {
        let mut big = commit("a", None);
        big.changes[0].value = Some(vec![0; MAX_PAYLOAD_LEN + 1]);
        let big = Record::Commit(big);

        let error = encode(&big).expect_err("too large to frame");

        assert!(
            matches!(error, LogError::RecordTooLarge { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_whole_frame_that_does_not_decode_is_corruption_even_last() {
        let first = frames(&[record("a", None)]);
        let mut bad = frames(&[record("b", None)]);
        bad[HEADER_LEN] = FORMAT_VERSION + 1;
        let crc = crc32fast::hash(&bad[HEADER_LEN..]);
        bad[LEN_PREFIX..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
        let bytes = [first.clone(), bad].concat();

        let error = scan(&bytes).expect_err("a checksummed frame is not torn");

        assert!(
            matches!(error, LogError::Corrupt { offset, .. } if offset == first.len() as u64),
            "{error:?}"
        );
    }

    #[test]
    fn a_commit_with_no_changes_round_trips() {
        let empty = Record::Commit(Commit {
            changes: Vec::new(),
            ..commit("a", None)
        });

        assert_eq!(
            scan(&frames(std::slice::from_ref(&empty)))
                .expect("scans")
                .records,
            vec![empty]
        );
    }

    #[test]
    fn branch_records_round_trip_between_commits() {
        let records = vec![
            record("a", None),
            Record::Branch(Branch {
                name: "event/halloween".to_owned(),
                from: "main".to_owned(),
            }),
            record("b", None),
        ];

        assert_eq!(scan(&frames(&records)).expect("scans").records, records);
    }

    fn group(keys: &[&str]) -> Record {
        Record::Group(keys.iter().map(|key| commit(key, None)).collect())
    }

    #[test]
    fn group_records_round_trip_between_other_records() {
        let records = vec![
            record("a", None),
            group(&["b", "c", "d"]),
            Record::Branch(Branch {
                name: "event/winter".to_owned(),
                from: "main".to_owned(),
            }),
            group(&["e"]),
        ];

        assert_eq!(scan(&frames(&records)).expect("scans").records, records);
    }

    #[test]
    fn erase_records_round_trip_with_and_without_approver() {
        let erase = |approved_by: Option<&str>| {
            Record::Erase(Erase {
                subject: b"player/42".to_vec(),
                key_fingerprint: [1, 2, 3, 4, 5, 6, 7, 255],
                who: "ops:privacy".to_owned(),
                why: "account deletion — 退会".to_owned(),
                approved_by: approved_by.map(str::to_owned),
                time: 1_800_000_000_000_001,
            })
        };
        let records = vec![
            record("a", None),
            erase(Some("ops:lead")),
            erase(None),
            group(&["b"]),
        ];

        assert_eq!(scan(&frames(&records)).expect("scans").records, records);
    }

    fn table(engine: Engine, approved_by: Option<&str>) -> Record {
        Record::Table(Table {
            name: b"position".to_vec(),
            engine,
            who: "ops:schema".to_owned(),
            why: "only the current position matters — 現在地".to_owned(),
            approved_by: approved_by.map(str::to_owned),
            time: 1_800_000_000_000_002,
        })
    }

    #[test]
    fn reject_records_round_trip() {
        let reject = Record::Reject(Reject {
            branch: "agent/support".to_owned(),
            head: [7; 32],
            who: "person:lead".to_owned(),
            why: "wrong ticket — 別件".to_owned(),
            time: 1_800_000_000_000_003,
        });
        let records = vec![record("a", None), reject, group(&["b"])];

        assert_eq!(scan(&frames(&records)).expect("scans").records, records);
    }

    #[test]
    fn table_records_round_trip_for_every_engine() {
        let records = vec![
            record("a", None),
            table(Engine::Latest, Some("ops:lead")),
            table(Engine::History, None),
            group(&["b"]),
        ];

        assert_eq!(scan(&frames(&records)).expect("scans").records, records);
    }

    /// An engine this build does not know is not guessed at: the frame is
    /// damage, as a frame of an unknown kind is.
    #[test]
    fn a_table_record_with_an_unknown_engine_is_corruption() {
        let declared = encode(&table(Engine::Latest, None)).expect("encodes");
        let mut payload = declared[HEADER_LEN..].to_vec();
        let engine_at = KIND_PREFIX + LEN_PREFIX + b"position".len();
        assert_eq!(payload[engine_at], 1, "the engine byte sits after the name");
        payload[engine_at] = 9;
        let len = u32::try_from(payload.len()).expect("fits");
        let crc = crc32fast::hash(&payload);
        let damaged = [&len.to_le_bytes()[..], &crc.to_le_bytes(), &payload].concat();
        let bytes = [
            frames(&[record("a", None)]),
            damaged,
            frames(&[record("b", None)]),
        ]
        .concat();

        assert!(scan(&bytes).is_err());
    }

    /// A group is one frame: cut anywhere inside it, none of its commits
    /// come back, and nothing before it is lost.
    #[test]
    fn a_group_cut_anywhere_comes_back_whole_or_not_at_all() {
        let before = record("a", None);
        let bytes = frames(&[before.clone(), group(&["b", "c", "d"])]);
        let start = encode(&before).expect("encodes").len();

        for cut in start..bytes.len() {
            let scan = scan(&bytes[..cut]).expect("a cut is a torn tail");

            assert_eq!(scan.records, vec![before.clone()], "cut at {cut}");
        }
    }

    #[test]
    fn commit_len_is_what_encoding_writes() {
        let plain = commit("a", None);
        let full = Commit {
            merged: Some([7; 32]),
            changes: vec![
                Edit {
                    key: b"kept".to_vec(),
                    value: Some(b"value".to_vec()),
                },
                Edit {
                    key: b"removed".to_vec(),
                    value: None,
                },
            ],
            ..commit("b", Some("ops:lead"))
        };
        let len = |c: &Commit| {
            commit_len(
                &c.branch,
                &c.who,
                &c.why,
                c.approved_by.as_deref(),
                c.merged.is_some(),
                &c.changes,
            )
        };

        for c in [&plain, &full] {
            let single = encode(&Record::Commit(c.clone())).expect("encodes");
            assert_eq!(single.len(), HEADER_LEN + KIND_PREFIX + len(c));
        }
        let group = encode(&Record::Group(vec![plain.clone(), full.clone()])).expect("encodes");
        assert_eq!(
            group.len(),
            HEADER_LEN + GROUP_OVERHEAD + len(&plain) + len(&full)
        );
    }
}
