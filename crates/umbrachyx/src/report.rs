//! Values, changes and commits as JSON, written the same way for an agent's
//! tools and for the server's history commands.
//!
//! A value is text when it is UTF-8 and `{"hex": ...}` otherwise. A value
//! whose subject was erased is `null` with `"erased": true`. Commits are
//! named by the start of their id, which `Store::resolve` accepts back.

use serde_json::{Value, json};

use crate::store::{Cell, CommitInfo, ValueChange, short};

pub(crate) fn shown(value: Option<&[u8]>) -> Value {
    match value {
        None => Value::Null,
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Value::String(text.to_owned()),
            Err(_) => json!({"hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()}),
        },
    }
}

/// One change to a key: the value it left, and the commit that made it.
pub(crate) fn change(cell: &Cell, info: &CommitInfo) -> Value {
    json!({
        "value": shown(cell.value.as_deref()),
        "erased": cell.erased,
        "commit": short(&info.id),
        "time_us": info.time,
        "who": info.provenance.who,
        "why": info.provenance.why,
        "approved_by": info.provenance.approved_by,
    })
}

/// One key whose value differs between two commits.
pub(crate) fn value_change(change: &ValueChange) -> Value {
    json!({
        "key": shown(Some(&change.key)),
        "before": shown(change.before.as_deref()),
        "after": shown(change.after.as_deref()),
        "erased": change.erased,
    })
}

pub(crate) fn commit(info: &CommitInfo) -> Value {
    json!({
        "commit": short(&info.id),
        "parent": info.parent.as_ref().map(short),
        "merged": info.merged.as_ref().map(short),
        "branch": info.branch,
        "time_us": info.time,
        "who": info.provenance.who,
        "why": info.provenance.why,
        "approved_by": info.provenance.approved_by,
    })
}
