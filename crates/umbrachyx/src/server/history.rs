//! Commands that read the history, for a person's tools: the branches, the
//! cells of a branch, one key's changes, a branch's commits, and the diff
//! between two commits, and the tables and the engines that hold them.
//! Names start with `UMBRA.`, as commands that Redis does not have usually
//! do.
//!
//! A table declared `latest` keeps only the current value of each key. Its
//! keys have no history: `UMBRA.HISTORY` on one answers an error whose first
//! word is `NOHISTORY`. `UMBRA.CELLS` leaves those tables out, and so does
//! `UMBRA.DIFF` where `main` kept no past for them; both name what they
//! left out in `omitted_tables`, empty when nothing was.
//!
//! Each answer is one bulk string of JSON, written as `report` writes it
//! for agents. Every walk stops after a page, because the committer's
//! writes wait while it runs. `next` names a key as JSON does; a key that
//! is not UTF-8 comes as `{"hex": ...}` and must be decoded before it is
//! sent back as `after`. Any branch can be read, not only the one served: reading
//! changes nothing, and main is what a person reviews a branch against.
//! The store answers on the committer's thread, after the commits sent
//! before, so these wait behind a sync and do not suit a hot path.

use serde_json::{Value, json};

use super::resp::Reply;
use crate::log::Engine;
use crate::report;
use crate::store::{CommitId, Inspect, StoreError, Writer, short};

/// The most cells or commits in one answer. A client asks for the next
/// cells after the last key it got.
pub const MAX_PAGE: usize = 1000;

/// A history command, its arguments read and owned so it can be sent to
/// the committer's thread.
enum Question {
    Branches,
    Tables,
    Cells {
        branch: String,
        after: Option<Vec<u8>>,
    },
    History {
        branch: String,
        key: Vec<u8>,
    },
    Log {
        branch: String,
    },
    Diff {
        from: String,
        to: String,
    },
}

/// Why a question has no answer.
enum Refusal {
    Store(StoreError),
    /// The key is in a table that keeps only its latest value.
    NoHistory(Vec<u8>),
}

impl From<StoreError> for Refusal {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl Refusal {
    fn reply(&self) -> Reply {
        match self {
            Self::Store(error) => Reply::Error(format!("ERR {error}")),
            Self::NoHistory(table) => Reply::Error(format!(
                "NOHISTORY table '{}' keeps only the latest value",
                String::from_utf8_lossy(table)
            )),
        }
    }
}

/// The reply to the history command `name`, or `None` if there is none by
/// that name. `name` is upper case; `served` is the branch this server
/// writes to.
pub(super) fn answer(writer: &Writer, served: &str, name: &str, args: &[Vec<u8>]) -> Option<Reply> {
    let question = match read(name, args)? {
        Ok(question) => question,
        Err(error) => return Some(Reply::Error(format!("ERR {error}"))),
    };
    let served = served.to_owned();
    Some(
        match writer.ask(move |store| ask(store, &served, &question)) {
            Ok(Ok(answer)) => Reply::Bulk(Some(answer.to_string().into_bytes())),
            Ok(Err(refusal)) => refusal.reply(),
            Err(error) => Reply::Error(format!("ERR {error}")),
        },
    )
}

fn read(name: &str, args: &[Vec<u8>]) -> Option<Result<Question, String>> {
    let question = match (name, args) {
        ("UMBRA.BRANCHES", []) => Ok(Question::Branches),
        ("UMBRA.TABLES", []) => Ok(Question::Tables),
        ("UMBRA.CELLS", [branch]) => text(branch).map(|branch| Question::Cells {
            branch,
            after: None,
        }),
        ("UMBRA.CELLS", [branch, after]) => text(branch).map(|branch| Question::Cells {
            branch,
            after: Some(after.clone()),
        }),
        ("UMBRA.HISTORY", [branch, key]) => text(branch).map(|branch| Question::History {
            branch,
            key: key.clone(),
        }),
        ("UMBRA.LOG", [branch]) => text(branch).map(|branch| Question::Log { branch }),
        ("UMBRA.DIFF", [from, to]) => {
            text(from).and_then(|from| text(to).map(|to| Question::Diff { from, to }))
        }
        (
            "UMBRA.BRANCHES" | "UMBRA.TABLES" | "UMBRA.CELLS" | "UMBRA.HISTORY" | "UMBRA.LOG"
            | "UMBRA.DIFF",
            _,
        ) => Err(format!(
            "wrong number of arguments for '{}' command",
            name.to_ascii_lowercase()
        )),
        _ => return None,
    };
    Some(question)
}

/// Branch and commit names are text; keys may be any bytes.
fn text(arg: &[u8]) -> Result<String, String> {
    String::from_utf8(arg.to_vec()).map_err(|_| "a branch or commit name must be UTF-8".to_owned())
}

fn ask(store: &dyn Inspect, served: &str, question: &Question) -> Result<Value, Refusal> {
    match question {
        Question::Branches => Ok(branches(store, served)),
        Question::Tables => Ok(tables(store)),
        Question::Cells { branch, after } => {
            require_branch(store, branch)?;
            Ok(cells(store, branch, after.as_deref()))
        }
        Question::History { branch, key } => {
            require_branch(store, branch)?;
            if let Some(table) = store.latest_table_of(key) {
                return Err(Refusal::NoHistory(table.to_vec()));
            }
            Ok(history(store, branch, key))
        }
        Question::Log { branch } => {
            require_branch(store, branch)?;
            Ok(log(store, branch))
        }
        Question::Diff { from, to } => Ok(diff(store, from, to)?),
    }
}

fn require_branch(store: &dyn Inspect, branch: &str) -> Result<(), StoreError> {
    if store.branches().contains(&branch) {
        Ok(())
    } else {
        Err(StoreError::UnknownBranch(branch.to_owned()))
    }
}

/// Every branch, and the one this server writes to, so a client knows
/// which branch its own writes are in.
fn branches(store: &dyn Inspect, served: &str) -> Value {
    let branches: Vec<Value> = store
        .branches()
        .into_iter()
        .map(|name| {
            let head = store.head(name).and_then(|id| store.commit_info(&id));
            json!({
                "name": name,
                "head": head.map(|info| short(&info.id)),
                "time_us": head.map(|info| info.time),
            })
        })
        .collect();
    json!({"branches": branches, "served": served})
}

/// The declared tables and their engines. A table not listed is held by
/// the default engine.
fn tables(store: &dyn Inspect) -> Value {
    let tables: Vec<Value> = store
        .tables()
        .iter()
        .map(|(name, engine)| json!({"name": report::shown(Some(name)), "engine": engine.name()}))
        .collect();
    json!({"default": Engine::History.name(), "tables": tables})
}

/// Table names as JSON shows keys.
fn names(tables: &[Vec<u8>]) -> Vec<Value> {
    tables
        .iter()
        .map(|name| report::shown(Some(name)))
        .collect()
}

/// One page of cells. `omitted_tables` names every latest-value table,
/// on every page and whichever keys the branch holds: their keys are never
/// listed here. `next` is the last key of the page when more follow,
/// to be sent back as `after`, and `null` on the last page.
fn cells(store: &dyn Inspect, branch: &str, after: Option<&[u8]>) -> Value {
    let mut cells = store.cells(branch, after, MAX_PAGE + 1);
    let has_more = cells.len() > MAX_PAGE;
    cells.truncate(MAX_PAGE);
    let next = if has_more {
        cells.last().map(|(key, _)| report::shown(Some(key)))
    } else {
        None
    };
    let cells: Vec<Value> = cells
        .iter()
        .filter_map(|(key, cell)| {
            let mut shown = report::change(cell, store.commit_info(&cell.changed_by)?);
            shown["key"] = report::shown(Some(key));
            Some(shown)
        })
        .collect();
    json!({
        "branch": branch,
        "cells": cells,
        "next": next,
        "omitted_tables": names(&store.latest_tables()),
    })
}

/// The newest changes, at most a page of them; `more` says older ones
/// were left out.
fn history(store: &dyn Inspect, branch: &str, key: &[u8]) -> Value {
    let history = store.history(branch, key, MAX_PAGE + 1);
    let changes: Vec<Value> = history
        .iter()
        .take(MAX_PAGE)
        .filter_map(|cell| Some(report::change(cell, store.commit_info(&cell.changed_by)?)))
        .collect();
    json!({
        "branch": branch,
        "key": report::shown(Some(key)),
        "changes": changes,
        "more": history.len() > MAX_PAGE,
    })
}

/// The newest commits of the branch's own line, at most a page of them.
fn log(store: &dyn Inspect, branch: &str) -> Value {
    let log = store.log(branch, MAX_PAGE + 1);
    let commits: Vec<Value> = log
        .iter()
        .take(MAX_PAGE)
        .map(|info| report::commit(info))
        .collect();
    json!({"branch": branch, "commits": commits, "more": log.len() > MAX_PAGE})
}

/// The first page of keys, in key order, whose values differ; `more` says
/// others were left out, and `omitted_tables` the tables it could not
/// diff. Working the diff out costs the number of changes, not the size of
/// the branches.
fn diff(store: &dyn Inspect, from: &str, to: &str) -> Result<Value, StoreError> {
    let (from_id, to_id) = (named(store, from)?, named(store, to)?);
    let changes = store.diff(from_id.as_ref(), to_id.as_ref())?;
    let omitted = store.omitted_tables(from_id.as_ref(), to_id.as_ref());
    let shown: Vec<Value> = changes
        .iter()
        .take(MAX_PAGE)
        .map(report::value_change)
        .collect();
    Ok(json!({
        "from": from,
        "to": to,
        "changes": shown,
        "more": changes.len() > MAX_PAGE,
        "omitted_tables": names(&omitted),
    }))
}

/// The commit `name` names, or `None` for a branch with no commits yet,
/// which holds nothing.
fn named(store: &dyn Inspect, name: &str) -> Result<Option<CommitId>, StoreError> {
    if store.branches().contains(&name) && store.head(name).is_none() {
        return Ok(None);
    }
    store.resolve(name).map(Some)
}
