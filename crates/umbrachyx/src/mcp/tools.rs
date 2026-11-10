//! The tools an agent can call. Each checks the token for every key it
//! touches before reading or writing it. Only `commit` writes, and only to
//! the token's branch.
//!
//! Keys must be text, since tokens name keys by text prefixes. Values are
//! returned as text when they are UTF-8 and as `{"hex": ...}` otherwise. A
//! value whose subject was erased is `null` with `"erased": true`. Erasing
//! is not a tool: a person does it.

use serde_json::{Value, json};

use super::Session;
use crate::capability::{Denied, Operation};
use crate::log::{Edit, Storage};
use crate::report::{self, shown};
use crate::store::{Clock, MAIN_BRANCH, Provenance, short};

/// The arguments `commit` takes. There is no branch among them.
const COMMIT_ARGUMENTS: [&str; 3] = ["set", "remove", "why"];

type Outcome = Result<Value, String>;

/// What `tools/list` returns.
pub(super) fn list() -> Value {
    json!([
        {
            "name": "get",
            "description": "Read one key's value. Reads the agent's own branch once it exists, \
                            otherwise main, unless `branch` names another.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "For example player/42/gems"},
                    "branch": {"type": "string"},
                },
                "required": ["key"],
            },
        },
        {
            "name": "history",
            "description": "Every change to one key, newest first: the value, the commit, \
                            who made it, why, and who approved it. A key of a table that \
                            keeps only its latest value has none, and is refused.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "branch": {"type": "string"},
                },
                "required": ["key"],
            },
        },
        {
            "name": "diff",
            "description": "The keys whose values differ between two commits or branches. \
                            Keys outside the agent's token are left out. Tables that keep \
                            only latest values have no past on main; a diff that needs it \
                            leaves them out and names them in omitted_tables.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "A branch, branch~N or a commit id"},
                    "to": {"type": "string"},
                },
                "required": ["from", "to"],
            },
        },
        {
            "name": "commit",
            "description": "Write values as one commit to the agent's own branch, made from main \
                            the first time. Nothing is written to main: a person reviews the \
                            branch and merges it. Every key must be inside the agent's token, \
                            or nothing is written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "set": {
                        "type": "object",
                        "additionalProperties": {"type": "string"},
                        "description": "Keys and their new values",
                    },
                    "remove": {"type": "array", "items": {"type": "string"}},
                    "why": {"type": "string", "description": "The reason, kept with the commit"},
                },
                "required": ["why"],
                "additionalProperties": false,
            },
        },
    ])
}

/// Runs the tool called `name`, or `None` if there is none.
pub(super) fn call<S: Storage, C: Clock>(
    session: &mut Session<S, C>,
    name: &str,
    arguments: &Value,
) -> Option<Outcome> {
    Some(match name {
        "get" => get(session, arguments),
        "history" => history(session, arguments),
        "diff" => diff(session, arguments),
        "commit" => commit(session, arguments),
        _ => return None,
    })
}

fn get<S: Storage, C: Clock>(session: &Session<S, C>, arguments: &Value) -> Outcome {
    let key = text(arguments, "key")?;
    let branch = read_branch(session, arguments)?;
    session
        .check(Operation::Read, &branch, key)
        .map_err(refused)?;
    let cell = session.store.cell(&branch, key.as_bytes());
    let erased = cell.as_ref().is_some_and(|cell| cell.erased);
    let value = cell.and_then(|cell| cell.value);
    Ok(json!({"branch": branch, "key": key, "value": shown(value.as_deref()), "erased": erased}))
}

fn history<S: Storage, C: Clock>(session: &Session<S, C>, arguments: &Value) -> Outcome {
    let key = text(arguments, "key")?;
    let branch = read_branch(session, arguments)?;
    session
        .check(Operation::Read, &branch, key)
        .map_err(refused)?;
    if let Some(table) = session.store.latest_table_of(key.as_bytes()) {
        return Err(format!(
            "table '{}' keeps only the latest value",
            String::from_utf8_lossy(table)
        ));
    }
    let changes: Vec<Value> = session
        .store
        .history(&branch, key.as_bytes())
        .into_iter()
        .filter_map(|cell| {
            let info = session.store.commit_info(&cell.changed_by)?;
            Some(report::change(&cell, info))
        })
        .collect();
    Ok(json!({"branch": branch, "key": key, "changes": changes}))
}

/// Keys the token cannot read are left out rather than refused, so a diff
/// across many keys shows the agent its own part. A token that allows
/// nothing at all, such as an expired one, is refused. `omitted_tables`
/// names the latest-value tables the diff could not cover, among those the
/// token may read.
fn diff<S: Storage, C: Clock>(session: &Session<S, C>, arguments: &Value) -> Outcome {
    let (from_name, to_name) = (text(arguments, "from")?, text(arguments, "to")?);
    let store = &session.store;
    let from = store.resolve(from_name).map_err(|e| e.to_string())?;
    let to = store.resolve(to_name).map_err(|e| e.to_string())?;
    let mut changes = Vec::new();
    for change in store
        .diff(Some(&from), Some(&to))
        .map_err(|e| e.to_string())?
    {
        let Ok(key) = std::str::from_utf8(&change.key) else {
            continue;
        };
        match session.check(Operation::Read, to_name, key) {
            Ok(()) => changes.push(report::value_change(&change)),
            Err(Denied::OutOfScope) => {}
            Err(denied) => return Err(refused(denied)),
        }
    }
    let mut omitted = Vec::new();
    for table in store.omitted_tables(Some(&from), Some(&to)) {
        if readable(session, to_name, &table)? {
            omitted.push(shown(Some(&table)));
        }
    }
    Ok(json!({
        "from": from_name,
        "to": to_name,
        "changes": changes,
        "omitted_tables": omitted,
    }))
}

/// Whether the token may read some key of `table` on `branch`, asked as
/// a read of each key [`probes`] makes, so every check of the token counts:
/// expiry, the operation, its narrowings. A token narrowed below what its
/// issuer granted may be told no for a table it reaches; it is never told
/// yes for one it does not. A token that allows nothing is refused.
fn readable<S: Storage, C: Clock>(
    session: &Session<S, C>,
    branch: &str,
    table: &[u8],
) -> Result<bool, String> {
    let holder = &session.holder;
    for key in probes(&holder.key_prefix, &holder.columns, table) {
        match session.check(Operation::Read, branch, &key) {
            Ok(()) => return Ok(true),
            Err(Denied::OutOfScope) => {}
            Err(denied) => return Err(refused(denied)),
        }
    }
    Ok(false)
}

/// Keys of `table` that the issuer's `prefix` and `columns` allow: one per
/// column, under the prefix where it lies inside the table, under the
/// table where the table lies inside the prefix. None when the two do not
/// meet, or the table is not text, as keys a token names are.
fn probes(prefix: &str, columns: &[String], table: &[u8]) -> Vec<String> {
    let Ok(table) = std::str::from_utf8(table) else {
        return Vec::new();
    };
    let root = format!("{table}/");
    let base = if root.starts_with(prefix) {
        root
    } else if prefix.starts_with(&root) {
        prefix.to_owned()
    } else {
        return Vec::new();
    };
    let separator = if base.ends_with('/') { "" } else { "/" };
    columns
        .iter()
        .map(|column| format!("{base}{separator}{column}"))
        .collect()
}

/// Every key is checked before anything is written, so a commit with one
/// key outside the token writes nothing, not even the branch.
fn commit<S: Storage, C: Clock>(session: &mut Session<S, C>, arguments: &Value) -> Outcome {
    let branch = session.holder.branch.clone();
    if let Some(other) = arguments.as_object().and_then(|fields| {
        fields
            .keys()
            .find(|name| !COMMIT_ARGUMENTS.contains(&name.as_str()))
    }) {
        return Err(format!(
            "commit takes no {other:?}: it writes to the token's branch, {branch}"
        ));
    }
    let why = text(arguments, "why")?.trim();
    if why.is_empty() {
        return Err("why must say why".to_owned());
    }
    let edits = edits(arguments)?;
    for edit in &edits {
        let key = std::str::from_utf8(&edit.key).expect("keys are read from JSON text");
        session
            .check(Operation::Write, &branch, key)
            .map_err(refused)?;
    }
    if !session.store.branches().any(|name| name == branch) {
        session
            .store
            .create_branch(&branch, MAIN_BRANCH)
            .map_err(|e| e.to_string())?;
    }
    let provenance = Provenance {
        who: session.holder.subject.clone(),
        why: why.to_owned(),
        approved_by: None,
    };
    let id = session
        .store
        .commit(&branch, edits, provenance)
        .map_err(|e| e.to_string())?;
    Ok(json!({"branch": branch, "commit": short(&id)}))
}

/// The keys `set` gives values and the keys `remove` takes away, in that
/// order. At least one.
fn edits(arguments: &Value) -> Result<Vec<Edit>, String> {
    let mut edits = Vec::new();
    match arguments.get("set") {
        None => {}
        Some(Value::Object(values)) => {
            for (key, value) in values {
                let Value::String(value) = value else {
                    return Err(format!("the value of {key:?} must be a string"));
                };
                edits.push(Edit {
                    key: key.as_bytes().to_vec(),
                    value: Some(value.as_bytes().to_vec()),
                });
            }
        }
        Some(_) => return Err("set must be an object of keys and values".to_owned()),
    }
    match arguments.get("remove") {
        None => {}
        Some(Value::Array(keys)) => {
            for key in keys {
                let Value::String(key) = key else {
                    return Err("remove must list keys as strings".to_owned());
                };
                edits.push(Edit {
                    key: key.as_bytes().to_vec(),
                    value: None,
                });
            }
        }
        Some(_) => return Err("remove must be a list of keys".to_owned()),
    }
    if edits.is_empty() {
        return Err("nothing to commit: give set or remove".to_owned());
    }
    Ok(edits)
}

/// The branch named in the arguments, or else the token's own branch once
/// it exists, or else main.
fn read_branch<S: Storage, C: Clock>(
    session: &Session<S, C>,
    arguments: &Value,
) -> Result<String, String> {
    let exists = |name: &str| session.store.branches().any(|branch| branch == name);
    match arguments.get("branch") {
        Some(Value::String(branch)) if exists(branch) => Ok(branch.clone()),
        Some(Value::String(branch)) => Err(format!("no branch {branch:?}")),
        Some(_) => Err("branch must be a string".to_owned()),
        None if exists(&session.holder.branch) => Ok(session.holder.branch.clone()),
        None => Ok(MAIN_BRANCH.to_owned()),
    }
}

fn text<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} is required and must be a string"))
}

fn refused(denied: Denied) -> String {
    format!("refused: {denied}")
}

#[cfg(test)]
mod tests {
    use super::probes;

    #[test]
    fn probes_lie_where_the_prefix_and_the_table_meet() {
        let columns = ["gems".to_owned(), "x".to_owned()];
        let cases: [(&str, &[u8], &[&str]); 8] = [
            ("", b"pos", &["pos/gems", "pos/x"]),
            ("po", b"pos", &["pos/gems", "pos/x"]),
            ("pos", b"pos", &["pos/gems", "pos/x"]),
            ("pos/", b"pos", &["pos/gems", "pos/x"]),
            ("pos/42/", b"pos", &["pos/42/gems", "pos/42/x"]),
            ("pos/4", b"pos", &["pos/4/gems", "pos/4/x"]),
            ("player/42/", b"pos", &[]),
            ("", &[0xff, 0xfe], &[]),
        ];
        for (prefix, table, expected) in cases {
            assert_eq!(probes(prefix, &columns, table), expected, "{prefix:?}");
        }
        assert_eq!(probes("pos", &columns, b"posx"), ["posx/gems", "posx/x"]);
        assert!(probes("po", &[], b"pos").is_empty(), "no columns, no keys");
    }
}
