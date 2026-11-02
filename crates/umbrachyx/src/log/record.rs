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
const FORMAT_VERSION: u8 = 2;
const KIND_COMMIT: u8 = 1;
const KIND_BRANCH: u8 = 2;
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
    match record {
        Record::Commit(commit) => encode_commit(commit),
        Record::Branch(branch) => {
            let mut payload = vec![FORMAT_VERSION, KIND_BRANCH];
            push_field(&mut payload, branch.name.as_bytes());
            push_field(&mut payload, branch.from.as_bytes());
            if payload.len() > MAX_PAYLOAD_LEN {
                return Err(LogError::RecordTooLarge { len: payload.len() });
            }
            Ok(payload)
        }
    }
}

fn encode_commit(record: &Commit) -> Result<Vec<u8>, LogError> {
    let mut payload = vec![FORMAT_VERSION, KIND_COMMIT];
    push_field(&mut payload, record.branch.as_bytes());
    push_field(&mut payload, record.who.as_bytes());
    push_field(&mut payload, record.why.as_bytes());
    push_optional(
        &mut payload,
        record.approved_by.as_deref().map(str::as_bytes),
    );
    payload.extend_from_slice(&record.time.to_le_bytes());
    let count = u32::try_from(record.changes.len()).map_err(|_| LogError::RecordTooLarge {
        len: record.changes.len(),
    })?;
    payload.extend_from_slice(&count.to_le_bytes());
    for write in &record.changes {
        push_field(&mut payload, &write.key);
        push_optional(&mut payload, write.value.as_deref());
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(LogError::RecordTooLarge { len: payload.len() });
        }
    }
    Ok(payload)
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
const MIN_PAYLOAD_LEN: usize = 2 + 2 * LEN_PREFIX;

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
/// Recovery-only. Most offsets are rejected on the length field alone, but a
/// long tail of arbitrary bytes can make this slow; the simulation disk will
/// measure that.
fn has_whole_frame_after_start(rest: &[u8]) -> bool {
    (1..rest.len()).any(|start| check_frame(&rest[start..]).is_ok())
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
        _ => return None,
    };
    cursor.rest.is_empty().then_some(record)
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
}
