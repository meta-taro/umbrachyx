//! A person deciding about the changes waiting, for a viewer: list them,
//! approve one into `main`, or reject it.
//!
//! `UMBRA.PENDING` reads, so any connection may ask. Each change is one
//! element, a flat array of field names and values, which a viewer that
//! draws Redis replies as tables shows without converting. `UMBRA.APPROVE`
//! and `UMBRA.REJECT` take the change's branch and the head the person saw;
//! they need a connection that sent `AUTH` with a token allowed to approve
//! every key the change touches. That token's holder is recorded as who
//! approved or rejected it. An agent's token writes, and a token that
//! writes is never issued the right to approve, so an agent cannot pass
//! its own change.

use std::time::SystemTime;

use biscuit_auth::PublicKey;

use super::MAX_PAGE;
use super::resp::Reply;
use super::session::Session;
use crate::capability::{Denied, Operation, Request};
use crate::store::{
    Approved, CommitId, Decided, MAIN_BRANCH, PendingChange, Provenance, StoreError, Writer, hex,
};

/// Decisions listed when no number is asked for.
const DEFAULT_DECISIONS: usize = 100;

/// How many conflicting keys a refused approval names.
const MAX_CONFLICTS_SHOWN: usize = 20;

/// The reply to one of these commands, or `None` for any other.
pub(super) fn answer(
    writer: &Writer,
    session: &Session,
    root: Option<PublicKey>,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    Some(match (name, args) {
        ("UMBRA.PENDING", []) => pending(writer),
        ("UMBRA.DECISIONS", []) => decisions(writer, DEFAULT_DECISIONS),
        ("UMBRA.DECISIONS", [limit]) => match std::str::from_utf8(limit)
            .ok()
            .and_then(|limit| limit.parse::<usize>().ok())
        {
            Some(limit) => decisions(writer, limit.min(MAX_PAGE)),
            None => Reply::Error("ERR the number of decisions is not a number".to_owned()),
        },
        ("UMBRA.APPROVE", [branch, head, reason]) => decide(
            writer,
            session,
            root,
            branch,
            head,
            reason,
            Decision::Approve,
        ),
        ("UMBRA.REJECT", [branch, head, reason]) => decide(
            writer,
            session,
            root,
            branch,
            head,
            reason,
            Decision::Reject,
        ),
        ("UMBRA.PENDING" | "UMBRA.APPROVE" | "UMBRA.REJECT" | "UMBRA.DECISIONS", _) => {
            Reply::Error(format!(
                "ERR wrong number of arguments for '{}' command",
                name.to_ascii_lowercase()
            ))
        }
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum Decision {
    Approve,
    Reject,
}

fn pending(writer: &Writer) -> Reply {
    match writer.ask(|store| store.pending()) {
        Ok(Ok(changes)) => Reply::Array(changes.iter().map(element).collect()),
        Ok(Err(error)) | Err(error) => Reply::Error(format!("ERR {error}")),
    }
}

/// The newest decisions, each one flat array of field names and values.
fn decisions(writer: &Writer, limit: usize) -> Reply {
    let bulk = |text: &str| Reply::Bulk(Some(text.as_bytes().to_vec()));
    match writer.ask(move |store| store.decisions(limit)) {
        Ok(decisions) => Reply::Array(
            decisions
                .iter()
                .map(|decision| {
                    let (word, commit) = match decision.decided {
                        Decided::Approved(merge) => ("approved", bulk(&hex(&merge))),
                        Decided::Rejected => ("rejected", Reply::Bulk(None)),
                    };
                    Reply::Array(vec![
                        bulk("decision"),
                        bulk(word),
                        bulk("branch"),
                        bulk(&decision.branch),
                        bulk("head"),
                        bulk(&hex(&decision.head)),
                        bulk("by"),
                        bulk(&decision.provenance.who),
                        bulk("why"),
                        bulk(&decision.provenance.why),
                        bulk("time"),
                        Reply::Integer(i64::try_from(decision.time).unwrap_or(i64::MAX)),
                        bulk("commit"),
                        commit,
                    ])
                })
                .collect(),
        ),
        Err(error) => Reply::Error(format!("ERR {error}")),
    }
}

fn element(change: &PendingChange) -> Reply {
    let bulk = |text: &str| Reply::Bulk(Some(text.as_bytes().to_vec()));
    let flag = |on: bool| Reply::Integer(i64::from(on));
    let time = |micros: u64| Reply::Integer(i64::try_from(micros).unwrap_or(i64::MAX));
    let tables = change
        .tables
        .iter()
        .flat_map(|(table, count)| {
            [
                Reply::Bulk(Some(table.clone())),
                Reply::Integer(i64::try_from(*count).unwrap_or(i64::MAX)),
            ]
        })
        .collect();
    Reply::Array(vec![
        bulk("branch"),
        bulk(&change.branch),
        bulk("head"),
        bulk(&hex(&change.head)),
        bulk("base"),
        match &change.base {
            Some(base) => bulk(&hex(base)),
            None => Reply::Bulk(None),
        },
        bulk("authors"),
        Reply::Array(change.authors.iter().map(|who| bulk(who)).collect()),
        bulk("why"),
        bulk(&change.why),
        bulk("first"),
        time(change.first),
        bulk("last"),
        time(change.last),
        bulk("tables"),
        Reply::Array(tables),
        bulk("main_moved"),
        flag(change.main_moved),
        bulk("conflicts"),
        flag(change.conflicts),
    ])
}

fn decide(
    writer: &Writer,
    session: &Session,
    root: Option<PublicKey>,
    branch: &[u8],
    head: &[u8],
    reason: &[u8],
    decision: Decision,
) -> Reply {
    let (Some((token, holder)), Some(_)) = (session.token(), root) else {
        return Reply::Error("NOAUTH Authentication required.".to_owned());
    };
    let branch = String::from_utf8_lossy(branch).into_owned();
    let reason = String::from_utf8_lossy(reason).into_owned();
    let head_name = String::from_utf8_lossy(head).into_owned();
    let asked = {
        let branch = branch.clone();
        writer.ask(move |store| {
            let head = store.resolve(&head_name)?;
            let keys = store.change_keys(&branch, &head)?;
            Ok::<_, StoreError>((head, keys))
        })
    };
    let (head, keys): (CommitId, _) = match asked {
        Ok(Ok(found)) => found,
        Ok(Err(error)) | Err(error) => return store_error(&error),
    };
    for key in &keys {
        let key = String::from_utf8_lossy(key);
        let allowed = token.authorize(&Request {
            operation: Operation::Approve,
            branch: MAIN_BRANCH,
            key: &key,
            column: "",
            purpose: &holder.purpose,
            now: SystemTime::now(),
        });
        if let Err(denied) = allowed {
            return no_permission(denied);
        }
    }
    let provenance = Provenance {
        who: holder.subject.clone(),
        why: reason,
        approved_by: None,
    };
    match decision {
        Decision::Approve => {
            match writer.decide(move |store| store.approve(&branch, &head, provenance)) {
                Ok(Ok(Approved::Commit(id))) => Reply::Bulk(Some(hex(&id).into_bytes())),
                Ok(Ok(Approved::Conflicts(conflicts))) => {
                    let keys: Vec<String> = conflicts
                        .iter()
                        .take(MAX_CONFLICTS_SHOWN)
                        .map(|c| String::from_utf8_lossy(&c.key).into_owned())
                        .collect();
                    let more = conflicts.len().saturating_sub(MAX_CONFLICTS_SHOWN);
                    let tail = if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    };
                    Reply::Error(format!("CONFLICT {}{tail}", keys.join(" ")))
                }
                Ok(Err(error)) | Err(error) => store_error(&error),
            }
        }
        Decision::Reject => {
            match writer.decide(move |store| store.reject(&branch, &head, provenance)) {
                Ok(Ok(())) => Reply::Simple("OK"),
                Ok(Err(error)) | Err(error) => store_error(&error),
            }
        }
    }
}

fn store_error(error: &StoreError) -> Reply {
    match error {
        StoreError::NotWaiting(_, why) => {
            Reply::Error(format!("NOTWAITING {} {error}", why.word()))
        }
        _ => Reply::Error(format!("ERR {error}")),
    }
}

fn no_permission(denied: Denied) -> Reply {
    Reply::Error(format!("NOPERM {denied}"))
}
