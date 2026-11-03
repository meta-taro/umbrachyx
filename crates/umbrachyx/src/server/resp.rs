//! The Redis serialization protocol (RESP2), as far as the server needs it:
//! commands arrive as arrays of bulk strings, and replies go back as simple
//! strings, errors, integers or bulk strings.
//!
//! Every length comes from the client, so each is checked before anything
//! is allocated for it.

use std::fmt;

/// More arguments than any command takes; a larger count is refused before
/// space is reserved for it.
pub const MAX_ARGS: usize = 1024;
/// All the arguments of one command together. This bounds what one
/// connection buffers, so 1,024 connections hold about 1 GiB at most. Well
/// under a record of the log, so a `SET` that is read always fits in one.
pub const MAX_COMMAND_LEN: usize = 1024 * 1024;
const _: () = assert!(MAX_COMMAND_LEN < crate::log::MAX_PAYLOAD_LEN);
/// The longest length line (`*` or `$`, digits): far more digits than any
/// accepted length needs.
const MAX_LINE: usize = 32;

/// A command: its name and arguments, each as the bytes the client sent.
pub type Command = Vec<Vec<u8>>;

/// Why a request could not be read. The connection cannot be read any
/// further, because where the next request starts is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Not an array of bulk strings: a type byte other than the one due.
    Unexpected { expected: u8, found: u8 },
    /// A length that is not a number, or not one allowed.
    BadLength,
    /// A line or a bulk string not ended by `\r\n`.
    MissingEnd,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unexpected { expected, found } => write!(
                f,
                "Protocol error: expected '{}', got '{}'",
                char::from(*expected),
                char::from(*found).escape_default()
            ),
            Self::BadLength => write!(f, "Protocol error: invalid length"),
            Self::MissingEnd => write!(f, "Protocol error: expected CRLF"),
        }
    }
}

/// Reads one command from the start of `input`. Returns the command and how
/// many bytes it took, or `None` if `input` ends before the command does.
pub fn parse(input: &[u8]) -> Result<Option<(Command, usize)>, ProtocolError> {
    let mut at = 0;
    let Some(count) = length(input, &mut at, b'*', MAX_ARGS)? else {
        return Ok(None);
    };
    // Where each argument lies. Nothing is copied until the whole command
    // has arrived: a command received in many reads is read again after
    // each, and copying every time would cost the square of its length.
    let mut args = Vec::with_capacity(count);
    let mut total = 0;
    for _ in 0..count {
        let Some(len) = length(input, &mut at, b'$', MAX_COMMAND_LEN - total)? else {
            return Ok(None);
        };
        let end = at + len;
        let Some(ending) = input.get(end..end + 2) else {
            return Ok(None);
        };
        if ending != b"\r\n" {
            return Err(ProtocolError::MissingEnd);
        }
        args.push(at..end);
        total += len;
        at = end + 2;
    }
    let command = args.into_iter().map(|arg| input[arg].to_vec()).collect();
    Ok(Some((command, at)))
}

/// Reads a line `<prefix><digits>\r\n` at `at` and moves `at` past it.
/// `None` if the line is not all there yet.
fn length(
    input: &[u8],
    at: &mut usize,
    prefix: u8,
    max: usize,
) -> Result<Option<usize>, ProtocolError> {
    let Some(&found) = input.get(*at) else {
        return Ok(None);
    };
    if found != prefix {
        return Err(ProtocolError::Unexpected {
            expected: prefix,
            found,
        });
    }
    let start = *at + 1;
    let window = &input[start..input.len().min(start + MAX_LINE)];
    let Some(cr) = window.iter().position(|&b| b == b'\r' || b == b'\n') else {
        return if window.len() == MAX_LINE {
            Err(ProtocolError::BadLength)
        } else {
            Ok(None)
        };
    };
    match input.get(start + cr..start + cr + 2) {
        None if window[cr] == b'\r' => return Ok(None),
        Some(b"\r\n") => {}
        _ => return Err(ProtocolError::MissingEnd),
    }
    let digits = &window[..cr];
    // `usize::from_str` would also take a leading `+`.
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(ProtocolError::BadLength);
    }
    let value = std::str::from_utf8(digits)
        .ok()
        .and_then(|digits| digits.parse::<usize>().ok())
        .filter(|&value| value <= max)
        .ok_or(ProtocolError::BadLength)?;
    *at = start + cr + 2;
    Ok(Some(value))
}

/// A reply to one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Simple(&'static str),
    Error(String),
    Integer(i64),
    /// A value, or nil when there is none.
    Bulk(Option<Vec<u8>>),
}

impl Reply {
    /// Appends the reply as RESP to `out`.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        match self {
            Self::Simple(text) => {
                out.push(b'+');
                out.extend_from_slice(text.as_bytes());
            }
            Self::Error(text) => {
                out.push(b'-');
                // A line break would end the error early and let the rest
                // be read as another reply.
                out.extend(text.bytes().map(|b| match b {
                    b'\r' | b'\n' => b' ',
                    b => b,
                }));
            }
            Self::Integer(value) => out.extend_from_slice(format!(":{value}").as_bytes()),
            Self::Bulk(None) => out.extend_from_slice(b"$-1"),
            Self::Bulk(Some(value)) => {
                out.extend_from_slice(format!("${}\r\n", value.len()).as_bytes());
                out.extend_from_slice(value);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&[u8]]) -> Vec<u8> {
        let mut bytes = format!("*{}\r\n", args.len()).into_bytes();
        for arg in args {
            bytes.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            bytes.extend_from_slice(arg);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes
    }

    fn unexpected(expected: u8, found: u8) -> ProtocolError {
        ProtocolError::Unexpected { expected, found }
    }

    fn written(reply: &Reply) -> Vec<u8> {
        let mut out = Vec::new();
        reply.write_to(&mut out);
        out
    }

    #[test]
    fn a_command_is_read_whole_and_reports_its_length() {
        let bytes = command(&[b"SET", b"key", b"a\r\nvalue"]);
        let (parsed, used) = parse(&bytes).expect("valid").expect("complete");
        assert_eq!(
            parsed,
            vec![b"SET".to_vec(), b"key".to_vec(), b"a\r\nvalue".to_vec()]
        );
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn pipelined_commands_are_read_one_at_a_time() {
        let mut bytes = command(&[b"GET", b"a"]);
        let first = bytes.len();
        bytes.extend(command(&[b"PING"]));
        let (parsed, used) = parse(&bytes).expect("valid").expect("complete");
        assert_eq!(
            (parsed, used),
            (vec![b"GET".to_vec(), b"a".to_vec()], first)
        );
        let (parsed, _) = parse(&bytes[used..]).expect("valid").expect("complete");
        assert_eq!(parsed, vec![b"PING".to_vec()]);
    }

    #[test]
    fn every_cut_short_command_waits_for_more() {
        let bytes = command(&[b"SET", b"key", b"value"]);
        for end in 0..bytes.len() {
            assert_eq!(parse(&bytes[..end]), Ok(None), "cut at {end}");
        }
    }

    #[test]
    fn an_empty_array_and_empty_arguments_are_read() {
        assert_eq!(parse(b"*0\r\n"), Ok(Some((Vec::new(), 4))));
        let bytes = command(&[b"GET", b""]);
        let (parsed, _) = parse(&bytes).expect("valid").expect("complete");
        assert_eq!(parsed, vec![b"GET".to_vec(), Vec::new()]);
    }

    #[test]
    fn malformed_input_is_refused() {
        let cases: [(&[u8], ProtocolError); 9] = [
            (b"GET a\r\n", unexpected(b'*', b'G')),
            (b"*1\r\n:1\r\n", unexpected(b'$', b':')),
            (b"*x\r\n", ProtocolError::BadLength),
            (b"*-1\r\n", ProtocolError::BadLength),
            (b"*\r\n", ProtocolError::BadLength),
            (b"*1\r\n$-1\r\n", ProtocolError::BadLength),
            (b"*1\r\n$+3\r\nabc\r\n", ProtocolError::BadLength),
            (b"*1\r\n$3\r\nabcde", ProtocolError::MissingEnd),
            (b"*1\n", ProtocolError::MissingEnd),
        ];
        for (input, error) in cases {
            assert_eq!(parse(input), Err(error), "{}", input.escape_ascii());
        }
    }

    #[test]
    fn lengths_past_the_limits_are_refused_before_their_bytes_arrive() {
        let args = format!("*{}\r\n", MAX_ARGS + 1);
        assert_eq!(parse(args.as_bytes()), Err(ProtocolError::BadLength));
        let arg = format!("*1\r\n${}\r\n", MAX_COMMAND_LEN + 1);
        assert_eq!(parse(arg.as_bytes()), Err(ProtocolError::BadLength));
        let half = MAX_COMMAND_LEN / 2;
        let mut two = format!("*2\r\n${half}\r\n").into_bytes();
        two.extend(vec![b'a'; half]);
        two.extend(format!("\r\n${}\r\n", half + 1).into_bytes());
        assert_eq!(parse(&two), Err(ProtocolError::BadLength));
        let digits = format!("*{}", "1".repeat(MAX_LINE));
        assert_eq!(parse(digits.as_bytes()), Err(ProtocolError::BadLength));
        let at_limit = format!("*1\r\n${MAX_COMMAND_LEN}\r\n");
        assert_eq!(parse(at_limit.as_bytes()), Ok(None));
    }

    #[test]
    fn replies_are_written_as_resp() {
        assert_eq!(written(&Reply::Simple("OK")), b"+OK\r\n");
        assert_eq!(written(&Reply::Error("ERR no".to_owned())), b"-ERR no\r\n");
        assert_eq!(written(&Reply::Integer(-3)), b":-3\r\n");
        assert_eq!(written(&Reply::Bulk(None)), b"$-1\r\n");
        assert_eq!(
            written(&Reply::Bulk(Some(b"a\r\nb".to_vec()))),
            b"$4\r\na\r\nb\r\n"
        );
    }

    #[test]
    fn an_error_cannot_break_its_line() {
        let out = written(&Reply::Error("ERR bad\r\n+OK".to_owned()));
        assert_eq!(out, b"-ERR bad  +OK\r\n");
    }
}
