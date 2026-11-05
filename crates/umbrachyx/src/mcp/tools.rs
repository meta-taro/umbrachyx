//! The tools an agent can call. Each checks the token for every key it
//! touches before reading or writing it.
//!
//! Keys must be text, since tokens name keys by text prefixes. Values are
//! returned as text when they are UTF-8 and as `{"hex": ...}` otherwise.

use serde_json::{Value, json};

use super::Session;
use crate::capability::{Denied, Operation};
use crate::log::Storage;
use crate::store::{Clock, short};

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
        _ => return None,
    })
}

fn get<S: Storage, C: Clock>(session: &Session<S, C>, arguments: &Value) -> Outcome {
    let key = text(arguments, "key")?;
    let branch = read_branch(session, arguments)?;
    session
        .check(Operation::Read, &branch, key)
        .map_err(refused)?;
    let value = session.store.get(&branch, key.as_bytes());
    Ok(json!({"branch": branch, "key": key, "value": shown(value.as_deref())}))
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
            Some(json!({
                "value": shown(cell.value.as_deref()),
                "commit": short(&info.id),
                "time_us": info.time,
                "who": info.provenance.who,
                "why": info.provenance.why,
                "approved_by": info.provenance.approved_by,
            }))
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
            Ok(()) => changes.push(json!({
                "key": key,
                "before": shown(change.before.as_deref()),
                "after": shown(change.after.as_deref()),
            })),
            Err(Denied::OutOfScope) => {}
            Err(denied) => return Err(refused(denied)),
        }
    }
    Ok(json!({"from": from_name, "to": to_name, "changes": changes}))
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
        None => Ok(crate::store::MAIN_BRANCH.to_owned()),
    }
}

fn text<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} is required and must be a string"))
}

fn shown(value: Option<&[u8]>) -> Value {
    match value {
        None => Value::Null,
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Value::String(text.to_owned()),
            Err(_) => json!({"hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()}),
        },
    }
}

fn refused(denied: Denied) -> String {
    format!("refused: {denied}")
}
