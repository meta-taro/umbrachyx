//! The on-disk form of one log record.
//!
//! A frame is `len: u32 LE | crc32: u32 LE | payload`, where the checksum
//! covers the payload only. The payload is the record's fields, each one
//! length-prefixed, so a reader never has to guess where a field ends.

use super::LogError;

/// One acknowledged change: the value and who wrote it, why, and who approved it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Record {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub who: String,
    pub why: String,
    pub approved_by: Option<String>,
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
const NO_APPROVER: u8 = 0;
const HAS_APPROVER: u8 = 1;

pub fn encode(record: &Record) -> Result<Vec<u8>, LogError> {
    let payload = encode_payload(record)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| LogError::RecordTooLarge { len: payload.len() })?;
    let crc = crc32fast::hash(&payload);
    Ok([&len.to_le_bytes()[..], &crc.to_le_bytes(), &payload].concat())
}

fn encode_payload(record: &Record) -> Result<Vec<u8>, LogError> {
    let approver: &[u8] = record.approved_by.as_deref().unwrap_or("").as_bytes();
    let fields: [&[u8]; 4] = [
        &record.key,
        &record.value,
        record.who.as_bytes(),
        record.why.as_bytes(),
    ];
    let len = fields.iter().map(|f| LEN_PREFIX + f.len()).sum::<usize>()
        + 1
        + record
            .approved_by
            .as_ref()
            .map_or(0, |_| LEN_PREFIX + approver.len());
    if len > MAX_PAYLOAD_LEN {
        return Err(LogError::RecordTooLarge { len });
    }

    let mut payload = Vec::with_capacity(len);
    for field in fields {
        push_field(&mut payload, field);
    }
    match record.approved_by {
        None => payload.push(NO_APPROVER),
        Some(_) => {
            payload.push(HAS_APPROVER);
            push_field(&mut payload, approver);
        }
    }
    Ok(payload)
}

fn push_field(payload: &mut Vec<u8>, field: &[u8]) {
    // Fits: the whole payload was checked against MAX_PAYLOAD_LEN (< 4 GiB).
    let len = field.len() as u32;
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

/// The smallest payload `encode` can produce: four empty length-prefixed
/// fields and the approver flag. Anything shorter is not a frame, which also
/// keeps a zeroed header (`len 0`, `crc32("") == 0`) from checking out.
const MIN_PAYLOAD_LEN: usize = 4 * LEN_PREFIX + 1;

fn read_frame(rest: &[u8], offset: u64) -> Result<Frame, LogError> {
    match check_frame(rest) {
        Ok(frame) => Ok(frame),
        Err(reason) if has_whole_frame_after_start(rest) => {
            Err(LogError::Corrupt { offset, reason })
        }
        Err(_) => Ok(Frame::TornTail),
    }
}

/// The frame at the start of `rest`, or why there is none.
fn check_frame(rest: &[u8]) -> Result<Frame, &'static str> {
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
    let record = decode_payload(payload).ok_or("malformed payload")?;
    Ok(Frame::Whole { record, len: end })
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
    let key = cursor.field()?.to_vec();
    let value = cursor.field()?.to_vec();
    let who = cursor.text()?;
    let why = cursor.text()?;
    let approved_by = match cursor.byte()? {
        NO_APPROVER => None,
        HAS_APPROVER => Some(cursor.text()?),
        _ => return None,
    };
    cursor.rest.is_empty().then_some(Record {
        key,
        value,
        who,
        why,
        approved_by,
    })
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
        String::from_utf8(self.field()?.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, approved_by: Option<&str>) -> Record {
        Record {
            key: key.as_bytes().to_vec(),
            value: vec![0, 1, 2, 255],
            who: "agent:inventory".to_owned(),
            why: "grant login bonus — 初回".to_owned(),
            approved_by: approved_by.map(str::to_owned),
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
        let mut big = record("a", None);
        big.value = vec![0; MAX_PAYLOAD_LEN + 1];

        let error = encode(&big).expect_err("too large to frame");

        assert!(
            matches!(error, LogError::RecordTooLarge { .. }),
            "{error:?}"
        );
    }
}
