//! What a replica and its primary send each other, as bytes: the ask
//! and its answer, and the ask for keys and its answer.

use super::super::resp::Reply;
use crate::keys::{FINGERPRINT_LEN, Fingerprint, KEY_LEN};
use crate::log::{Chain, LogId, MAX_PAYLOAD_LEN, record};
use crate::store::follow::{Ask, Held, KeyAnswer, MAX_SENT, Refusal, Sent};

pub(super) const LOG_AFTER: &str = "UMBRA.LOG.AFTER";
pub(super) const KEYS: &str = "UMBRA.KEYS";
/// The most keys asked for with one command. A megabyte of records
/// could name more subjects; a replica then asks again.
pub(super) const MAX_KEYS_ASKED: usize = 1_024;
/// In front of each key's answer: the key follows, it was erased, or
/// the primary never had it. Then the fingerprint it is about.
const KEY_FOLLOWS: u8 = 1;
const KEY_ERASED: u8 = 2;
const KEY_UNKNOWN: u8 = 3;
/// `after` and `records`, in front of the frames of an answer.
const ANSWER_HEAD: usize = 16;
/// The longest answer: a record is never cut, so one larger than a
/// primary otherwise sends is an answer on its own, and a record is at
/// most this long.
const MAX_ANSWER: usize = ANSWER_HEAD + record::HEADER_LEN + MAX_PAYLOAD_LEN;
const _: () = assert!(MAX_SENT <= record::HEADER_LEN + MAX_PAYLOAD_LEN);

pub(super) fn keys_value(answers: &[KeyAnswer]) -> Vec<u8> {
    let mut value = Vec::new();
    for answer in answers {
        let tag = match answer {
            KeyAnswer::Key(_) => KEY_FOLLOWS,
            KeyAnswer::Erased(_) => KEY_ERASED,
            KeyAnswer::Unknown(_) => KEY_UNKNOWN,
        };
        value.push(tag);
        value.extend_from_slice(&answer.fingerprint());
        if let KeyAnswer::Key(key) = answer {
            value.extend_from_slice(key);
        }
    }
    value
}

/// The command that asks for keys.
pub(super) fn keys_command(wanted: &[Fingerprint]) -> Vec<u8> {
    let mut command =
        format!("*{}\r\n${}\r\n{KEYS}\r\n", wanted.len() + 1, KEYS.len()).into_bytes();
    for wanted in wanted {
        let arg = hex(wanted);
        command.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    command
}

/// Reads the reply to an ask for keys from the start of `input`.
pub(super) fn read_keys(input: &[u8]) -> Result<Arrived<Vec<KeyAnswer>>, String> {
    const NOT_KEYS: &str = "the answer is not keys as a primary sends them";
    let Some(line_end) = input.windows(2).position(|pair| pair == b"\r\n") else {
        return Ok(Arrived::Part(None));
    };
    if line_end == 0 {
        return Err(NOT_KEYS.to_owned());
    }
    let line = &input[1..line_end];
    if input[0] != b'$' {
        return Err(String::from_utf8_lossy(line).into_owned());
    }
    let most = MAX_KEYS_ASKED * (1 + FINGERPRINT_LEN + KEY_LEN);
    let len: usize = number(line).filter(|&len| len <= most).ok_or(NOT_KEYS)?;
    let start = line_end + 2;
    let Some(value) = input.get(start..start + len + 2) else {
        return Ok(Arrived::Part(Some(start + len + 2)));
    };
    let mut rest = &value[..len];
    let mut answers = Vec::new();
    while let Some((&tag, after)) = rest.split_first() {
        let (about, after) = after.split_at_checked(FINGERPRINT_LEN).ok_or(NOT_KEYS)?;
        let about: Fingerprint = about.try_into().expect("FINGERPRINT_LEN bytes");
        rest = after;
        answers.push(match tag {
            KEY_FOLLOWS => {
                let (key, after) = rest.split_at_checked(KEY_LEN).ok_or(NOT_KEYS)?;
                rest = after;
                KeyAnswer::Key(key.try_into().expect("KEY_LEN bytes"))
            }
            KEY_ERASED => KeyAnswer::Erased(about),
            KEY_UNKNOWN => KeyAnswer::Unknown(about),
            _ => return Err(NOT_KEYS.to_owned()),
        });
    }
    Ok(Arrived::Whole(answers))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(super) fn unhex<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != 2 * N {
        return None;
    }
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(text.chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

fn number<T: std::str::FromStr>(text: &[u8]) -> Option<T> {
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// The command that asks.
pub(super) fn ask_command(ask: &Ask) -> Vec<u8> {
    let args = [
        LOG_AFTER.to_owned(),
        hex(&ask.held.log),
        ask.held.records.to_string(),
        hex(&ask.held.chain),
        ask.max_bytes.to_string(),
    ];
    let mut command = format!("*{}\r\n", args.len()).into_bytes();
    for arg in args {
        command.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    command
}

pub(super) fn read_ask(args: &[Vec<u8>]) -> Result<Ask, String> {
    let [log, records, chain, max_bytes] = args else {
        return Err(format!(
            "wrong number of arguments for '{}' command",
            LOG_AFTER.to_ascii_lowercase()
        ));
    };
    let log: LogId = unhex(log).ok_or("the log's name is 32 hex digits")?;
    let chain: Chain = unhex(chain).ok_or("the chain is 64 hex digits")?;
    let records = number(records).ok_or("the number of records held is not a number")?;
    let max_bytes = number(max_bytes).ok_or("the most bytes to send is not a number")?;
    Ok(Ask {
        held: Held {
            log,
            records,
            chain,
        },
        max_bytes,
    })
}

/// A refusal is an error whose first word names it; what is sent is one
/// value: where the frames start and how many records the primary
/// holds, eight bytes each, then the frames.
pub(super) fn answer_reply(answer: Result<Sent, Refusal>) -> Reply {
    match answer {
        Ok(sent) => {
            let mut value = Vec::with_capacity(ANSWER_HEAD + sent.frames.len());
            value.extend_from_slice(&sent.after.to_le_bytes());
            value.extend_from_slice(&sent.records.to_le_bytes());
            value.extend_from_slice(&sent.frames);
            Reply::Bulk(Some(value))
        }
        Err(refusal) => {
            let word = match refusal {
                Refusal::AnotherLog => "ANOTHERLOG".to_owned(),
                Refusal::Fewer { records } => format!("FEWER {records}"),
                Refusal::Diverged => "DIVERGED".to_owned(),
            };
            Reply::Error(format!("{word} {refusal}"))
        }
    }
}

/// What the start of `input` holds of a reply.
pub(super) enum Arrived<T> {
    /// Not all of it yet. With this many bytes in all it would be whole,
    /// if that can be told already.
    Part(Option<usize>),
    Whole(T),
}

/// Reads the reply to an ask from the start of `input`. An error is a
/// reply that is no answer: the server said why, or it is not one of
/// ours.
/// The most that is kept of a line a server refused with.
const MAX_REFUSAL_CHARS: usize = 300;

pub(super) fn read_answer(input: &[u8]) -> Result<Arrived<Result<Sent, Refusal>>, String> {
    let Some(line_end) = input.windows(2).position(|pair| pair == b"\r\n") else {
        return Ok(Arrived::Part(None));
    };
    if line_end == 0 {
        return Err("the answer is not one a primary sends".to_owned());
    }
    let line = &input[1..line_end];
    match input[0] {
        b'-' => {
            // It ends up in this server's own messages: no longer than a
            // line of them, and nothing that is not print.
            let text: String = String::from_utf8_lossy(line)
                .chars()
                .filter(|letter| !letter.is_control())
                .take(MAX_REFUSAL_CHARS)
                .collect();
            let mut words = text.split(' ');
            match (words.next(), words.next()) {
                (Some("ANOTHERLOG"), _) => Ok(Arrived::Whole(Err(Refusal::AnotherLog))),
                (Some("DIVERGED"), _) => Ok(Arrived::Whole(Err(Refusal::Diverged))),
                (Some("FEWER"), Some(records)) => match records.parse() {
                    Ok(records) => Ok(Arrived::Whole(Err(Refusal::Fewer { records }))),
                    Err(_) => Err(text),
                },
                _ => Err(text),
            }
        }
        b'$' => {
            let len: usize = number(line)
                .filter(|&len| (ANSWER_HEAD..=MAX_ANSWER).contains(&len))
                .ok_or("the answer is not one a primary sends")?;
            let start = line_end + 2;
            // With the line end after it, or the next answer on this
            // connection would start with one.
            let Some(value) = input.get(start..start + len + 2) else {
                return Ok(Arrived::Part(Some(start + len + 2)));
            };
            let value = &value[..len];
            let word = |at: usize| {
                let mut bytes = [0; 8];
                bytes.copy_from_slice(&value[at..at + 8]);
                u64::from_le_bytes(bytes)
            };
            Ok(Arrived::Whole(Ok(Sent {
                after: word(0),
                records: word(8),
                frames: value[ANSWER_HEAD..].to_vec(),
            })))
        }
        _ => Err("the answer is not one a primary sends".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::resp;
    use super::super::ONLY_REPLICAS;
    use super::*;

    fn ask() -> Ask {
        Ask {
            held: Held {
                log: *b"a log's own name",
                records: 12_345,
                chain: [0xa7; 32],
            },
            max_bytes: 4096,
        }
    }

    fn sent() -> Sent {
        Sent {
            after: 12_345,
            records: 12_400,
            frames: b"frames, with \r\n in them".to_vec(),
        }
    }

    fn wire(answer: Result<Sent, Refusal>) -> Vec<u8> {
        let mut out = Vec::new();
        answer_reply(answer).write_to(&mut out);
        out
    }

    fn arrived(input: &[u8]) -> Option<Result<Sent, Refusal>> {
        match read_answer(input).expect("is an answer") {
            Arrived::Whole(answer) => Some(answer),
            Arrived::Part(_) => None,
        }
    }

    #[test]
    fn an_ask_is_read_as_it_was_written() {
        let command = ask_command(&ask());

        let (parsed, len) = super::super::resp::parse(&command)
            .expect("is a command")
            .expect("is whole");

        assert_eq!(len, command.len());
        assert_eq!(parsed[0], LOG_AFTER.as_bytes());
        assert_eq!(read_ask(&parsed[1..]), Ok(ask()));
    }

    #[test]
    fn an_ask_that_is_not_one_is_refused_with_what_is_wrong() {
        let mut args: Vec<Vec<u8>> = super::super::resp::parse(&ask_command(&ask()))
            .expect("is a command")
            .expect("is whole")
            .0[1..]
            .to_vec();
        args[0].pop();

        assert_eq!(
            read_ask(&args),
            Err("the log's name is 32 hex digits".to_owned())
        );
        assert!(read_ask(&args[1..]).is_err());
    }

    #[test]
    fn every_answer_is_read_as_it_was_written() {
        for answer in [
            Ok(sent()),
            Ok(Sent {
                frames: Vec::new(),
                ..sent()
            }),
            Err(Refusal::AnotherLog),
            Err(Refusal::Fewer { records: 7 }),
            Err(Refusal::Diverged),
        ] {
            assert_eq!(arrived(&wire(answer.clone())), Some(answer));
        }
    }

    #[test]
    fn an_answer_cut_short_anywhere_is_waited_for() {
        let whole = wire(Ok(sent()));

        for len in 1..whole.len() {
            assert_eq!(arrived(&whole[..len]), None, "{len} bytes");
        }
    }

    #[test]
    fn a_reply_that_is_no_answer_is_said_as_it_came() {
        let mut out = Vec::new();
        Reply::Error(ONLY_REPLICAS.to_owned()).write_to(&mut out);

        let said = read_answer(&out).err();

        assert_eq!(said.as_deref(), Some(ONLY_REPLICAS));
        assert!(read_answer(b"+OK\r\n").is_err());
        // Not even the sign a reply starts with.
        assert!(read_answer(b"\r\n").is_err());
        assert!(read_keys(b"\r\n").is_err());
        assert!(read_answer(b"$3\r\nabc\r\n").is_err());
    }

    fn key_answers() -> Vec<KeyAnswer> {
        vec![
            KeyAnswer::Key([0x5a; KEY_LEN]),
            KeyAnswer::Erased(*b"erased!!"),
            KeyAnswer::Unknown(*b"\r\nknown?"),
            KeyAnswer::Key([0x0d; KEY_LEN]),
        ]
    }

    fn keys_wire(answers: &[KeyAnswer]) -> Vec<u8> {
        let mut out = Vec::new();
        Reply::Bulk(Some(keys_value(answers))).write_to(&mut out);
        out
    }

    #[test]
    fn keys_are_read_as_they_were_written_and_waited_for_when_cut_short() {
        let wire = keys_wire(&key_answers());

        let whole = read_keys(&wire).expect("is keys");
        let none = read_keys(&keys_wire(&[])).expect("is keys");

        assert!(matches!(whole, Arrived::Whole(answers) if answers == key_answers()));
        assert!(matches!(none, Arrived::Whole(answers) if answers.is_empty()));
        for cut in 0..wire.len() {
            let part = read_keys(&wire[..cut]).expect("may yet be keys");
            assert!(matches!(part, Arrived::Part(_)), "cut at {cut}");
        }
    }

    #[test]
    fn what_is_not_keys_is_said_as_it_came_and_nothing_of_it_kept() {
        let mut no_such_tag = keys_value(&key_answers());
        no_such_tag[0] = 9;
        let mut cut_inside = keys_value(&key_answers());
        cut_inside.truncate(cut_inside.len() - 1);
        let wire = |value: Vec<u8>| {
            let mut out = Vec::new();
            Reply::Bulk(Some(value)).write_to(&mut out);
            out
        };

        let refused = read_keys(b"-ERR this port answers replicas only: UMBRA.LOG.AFTER\r\n");
        let tag = read_keys(&wire(no_such_tag));
        let short = read_keys(&wire(cut_inside));
        let too_long = read_keys(b"$999999999\r\n");

        assert!(matches!(refused, Err(why) if why.contains("replicas only")));
        assert!(tag.is_err());
        assert!(short.is_err());
        assert!(too_long.is_err());
    }

    #[test]
    fn the_command_that_asks_for_keys_names_each_by_its_fingerprint() {
        let command = keys_command(&[*b"\x00\x01\x02\x03\x04\x05\x06\xff", [0xab; 8]]);

        let (parsed, len) = resp::parse(&command).expect("parses").expect("is whole");

        assert_eq!(len, command.len());
        assert_eq!(
            parsed,
            [
                b"UMBRA.KEYS".to_vec(),
                b"00010203040506ff".to_vec(),
                b"abababababababab".to_vec()
            ]
        );
    }
}
