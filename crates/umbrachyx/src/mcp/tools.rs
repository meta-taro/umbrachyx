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
                            who made it, why, and who approved it.",
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
                            Keys outside the agent's token are left out.",
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
/// nothing at all, such as an expired one, is refused.
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
    Ok(json!({"from": from_name, "to": to_name, "changes": changes}))
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
