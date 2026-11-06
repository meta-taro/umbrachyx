//! The commands a viewer sends to list keys and open one: `SCAN`, `TYPE`
//! and `TTL`, on the branch the server serves.
//!
//! Every value is a string and none expires, so `TYPE` says `string` and
//! `TTL` says `-1` for a key that holds a value, erased or not, and `none`
//! and `-2` for one that does not, as Redis does. They read the latest view,
//! as `GET` does, so they never wait for the committer.
//!
//! A `SCAN` cursor counts the entries of the branch's tree already walked.
//! A key added or removed before the cursor between two calls moves the
//! rest by one, so a walk may then return a key twice or miss one, which
//! Redis also allows for keys changed during a walk. Finding where a cursor
//! points reads the entries before it.

use super::resp::Reply;
use crate::store::View;

/// Entries one `SCAN` reads when it is not given `COUNT`, as in Redis.
const DEFAULT_COUNT: usize = 10;

/// The reply to `name`, or `None` if it is not one of these. `name` is
/// upper case.
pub(super) fn answer(view: &View, branch: &str, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    Some(match (name, args) {
        ("TYPE", [key]) => Reply::Simple(if view.contains(branch, key) {
            "string"
        } else {
            "none"
        }),
        ("TTL", [key]) => Reply::Integer(if view.contains(branch, key) { -1 } else { -2 }),
        ("SCAN", [cursor, options @ ..]) => scan(view, branch, cursor, options),
        ("TYPE" | "TTL" | "SCAN", _) => Reply::Error(format!(
            "ERR wrong number of arguments for '{}' command",
            name.to_ascii_lowercase()
        )),
        _ => return None,
    })
}

fn scan(view: &View, branch: &str, cursor: &[u8], options: &[Vec<u8>]) -> Reply {
    let Some(skip) = std::str::from_utf8(cursor)
        .ok()
        .and_then(|cursor| cursor.parse::<usize>().ok())
    else {
        return Reply::Error("ERR invalid cursor".to_owned());
    };
    let Some((pattern, count)) = scan_options(options) else {
        return Reply::Error("ERR syntax error".to_owned());
    };
    let (keys, has_more) = view.keys(branch, skip, count);
    let next = if has_more { skip + count } else { 0 };
    let keys = keys
        .into_iter()
        .filter(|key| pattern.is_none_or(|pattern| matches(pattern, key)))
        .map(|key| Reply::Bulk(Some(key.to_vec())))
        .collect();
    Reply::Array(vec![
        Reply::Bulk(Some(next.to_string().into_bytes())),
        Reply::Array(keys),
    ])
}

/// `MATCH pattern` and `COUNT n`, in either order. Anything else, such as
/// `TYPE`, is refused rather than ignored.
fn scan_options(options: &[Vec<u8>]) -> Option<(Option<&[u8]>, usize)> {
    let mut pattern = None;
    let mut count = DEFAULT_COUNT;
    let mut rest = options;
    while let [option, value, tail @ ..] = rest {
        match option.to_ascii_uppercase().as_slice() {
            b"MATCH" => pattern = Some(value.as_slice()),
            b"COUNT" => {
                count = std::str::from_utf8(value).ok()?.parse().ok()?;
                if count == 0 {
                    return None;
                }
            }
            _ => return None,
        }
        rest = tail;
    }
    rest.is_empty().then_some((pattern, count))
}

/// Whether `key` matches a glob as Redis writes them: `*` for any run, `?`
/// for one byte, `[abc]`, `[^abc]` and `[a-z]` for one of a set, and `\` to
/// take the next byte as it is.
fn matches(pattern: &[u8], key: &[u8]) -> bool {
    match pattern.split_first() {
        None => key.is_empty(),
        Some((b'*', rest)) => (0..=key.len()).any(|skip| matches(rest, &key[skip..])),
        Some((b'?', rest)) => !key.is_empty() && matches(rest, &key[1..]),
        Some((b'[', rest)) => {
            let Some((&first, key_rest)) = key.split_first() else {
                return false;
            };
            match class(rest, first) {
                Some((true, after)) => matches(after, key_rest),
                Some((false, _)) => false,
                // No closing `]`: the `[` is an ordinary byte.
                None => first == b'[' && matches(rest, key_rest),
            }
        }
        Some((b'\\', [escaped, rest @ ..])) => {
            key.first() == Some(escaped) && matches(rest, &key[1..])
        }
        Some((&byte, rest)) => key.first() == Some(&byte) && matches(rest, &key[1..]),
    }
}

/// Whether `byte` is in the set that starts `pattern` (just after `[`), and
/// the pattern after its `]`; `None` if there is no `]`.
fn class(pattern: &[u8], byte: u8) -> Option<(bool, &[u8])> {
    let (negated, mut rest) = match pattern.split_first() {
        Some((b'^', rest)) => (true, rest),
        _ => (false, pattern),
    };
    let mut found = false;
    loop {
        match rest {
            [] => return None,
            [b']', after @ ..] => return Some((found != negated, after)),
            [b'\\', escaped, after @ ..] => {
                found |= *escaped == byte;
                rest = after;
            }
            [low, b'-', high, after @ ..] if *high != b']' => {
                let (low, high) = ((*low).min(*high), (*low).max(*high));
                found |= (low..=high).contains(&byte);
                rest = after;
            }
            [single, after @ ..] => {
                found |= *single == byte;
                rest = after;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn globs_match_as_redis_does() {
        let cases: [(&str, &str, bool); 14] = [
            ("*", "", true),
            ("player/*", "player/7/gems", true),
            ("player/*", "players/7", false),
            ("h?llo", "hello", true),
            ("h?llo", "hllo", false),
            ("h[ae]llo", "hallo", true),
            ("h[ae]llo", "hillo", false),
            ("h[^e]llo", "hallo", true),
            ("h[^e]llo", "hello", false),
            ("h[a-b]llo", "hbllo", true),
            ("h[a-b]llo", "hcllo", false),
            ("a\\*b", "a*b", true),
            ("a\\*b", "axb", false),
            ("a[b", "a[b", true),
        ];
        for (pattern, key, expected) in cases {
            assert_eq!(
                matches(pattern.as_bytes(), key.as_bytes()),
                expected,
                "{pattern} against {key}"
            );
        }
    }
}
