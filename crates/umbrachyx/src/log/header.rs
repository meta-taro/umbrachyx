//! What a log file starts with: 80 bytes that say it is a log, in which
//! layout, which log it is, and where in that log the file begins.
//!
//! `magic: 8 | version: u32 LE | first record: u64 LE | chain: 32 |
//! id: 16 | created: u64 LE | crc32: u32 LE`, the checksum over all that
//! comes before it.
//!
//! Today a log is one file, which begins at record 0. The header already
//! says where a file begins and what the chain was before it, so that a
//! log can later be laid out in several files, each of which says on its
//! own which log it belongs to and where it goes, and a log written now
//! is simply the first of them.

use super::Chain;

pub const HEADER_LEN: usize = 80;
const MAGIC: [u8; 8] = *b"UMBRALOG";
/// Bumped when the header's own layout changes. The records carry their
/// own version, in each payload.
const VERSION: u32 = 1;
pub const ID_LEN: usize = 16;
const CHECKED_LEN: usize = HEADER_LEN - 4;

/// Names a log: sixteen random bytes, made once, that every copy of the
/// log keeps.
pub type LogId = [u8; ID_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// How many records of the log come before this file.
    pub first_record: u64,
    /// The chain over those records.
    pub chain_before: Chain,
    pub id: LogId,
    /// Microseconds since the Unix epoch, when the log was made.
    pub created: u64,
}

/// What the start of a file turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Header(Header),
    /// It says it is a log, in a layout this version does not know.
    Newer(u32),
    /// Not a header: nothing, zeros, the torn start of one, or other bytes.
    None,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut bytes = [0; HEADER_LEN];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.first_record.to_le_bytes());
        bytes[20..52].copy_from_slice(&self.chain_before);
        bytes[52..68].copy_from_slice(&self.id);
        bytes[68..76].copy_from_slice(&self.created.to_le_bytes());
        let crc = crc32fast::hash(&bytes[..CHECKED_LEN]);
        bytes[CHECKED_LEN..].copy_from_slice(&crc.to_le_bytes());
        bytes
    }
}

/// Reads the start of a log file.
pub fn read(bytes: &[u8]) -> Start {
    let Some(header) = bytes.get(..HEADER_LEN) else {
        return Start::None;
    };
    if header[..8] != MAGIC {
        return Start::None;
    }
    let version = u32::from_le_bytes(word(&header[8..12]));
    let crc = u32::from_le_bytes(word(&header[CHECKED_LEN..]));
    if version != VERSION || crc != crc32fast::hash(&header[..CHECKED_LEN]) {
        // A newer layout may keep its checksum elsewhere, so the version
        // is believed without one. A damaged version field reads as newer
        // too: either way the file is not opened.
        return if version > VERSION {
            Start::Newer(version)
        } else {
            Start::None
        };
    }
    Start::Header(Header {
        first_record: u64::from_le_bytes(word(&header[12..20])),
        chain_before: word(&header[20..52]),
        id: word(&header[52..68]),
        created: u64::from_le_bytes(word(&header[68..76])),
    })
}

fn word<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut word = [0; N];
    word.copy_from_slice(bytes);
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            first_record: 0,
            chain_before: [7; 32],
            id: [9; ID_LEN],
            created: 1_794_570_631_599_977,
        }
    }

    #[test]
    fn a_header_reads_back_as_written() {
        assert_eq!(read(&header().encode()), Start::Header(header()));
    }

    #[test]
    fn a_header_is_eighty_bytes_that_start_by_saying_what_they_are() {
        let bytes = header().encode();

        assert_eq!(bytes.len(), 80);
        assert_eq!(&bytes[..8], b"UMBRALOG");
    }

    #[test]
    fn any_flipped_bit_stops_it_reading_as_this_header() {
        let written = header().encode();

        for bit in 0..HEADER_LEN * 8 {
            let mut bytes = written;
            bytes[bit / 8] ^= 1 << (bit % 8);

            assert_ne!(read(&bytes), Start::Header(header()), "bit {bit}");
            assert!(!matches!(read(&bytes), Start::Header(_)), "bit {bit}");
        }
    }

    #[test]
    fn a_later_version_is_told_apart_from_bytes_that_are_no_header() {
        let mut bytes = header().encode();
        bytes[8..12].copy_from_slice(&2_u32.to_le_bytes());

        assert_eq!(read(&bytes), Start::Newer(2));
    }

    #[test]
    fn nothing_zeros_a_cut_header_and_other_bytes_are_no_header() {
        let written = header().encode();

        assert_eq!(read(&[]), Start::None);
        assert_eq!(read(&[0; 4096]), Start::None);
        assert_eq!(read(&written[..HEADER_LEN - 1]), Start::None);
        assert_eq!(read(&[b'x'; HEADER_LEN]), Start::None);
    }
}
