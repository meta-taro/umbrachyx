//! A server backing its store up as it serves.
//!
//! Whoever starts the server names the place and how often; no client
//! can ask for a backup or say where one goes, so nothing that reaches
//! the port can make the server write a file. A thread of its own takes
//! the backups, through the same turns the writers take: once for the
//! moment a backup is to hold, then once for each part of the log it
//! copies. Writers wait for a part to be read, and never for the place
//! the backup is kept, however slow that is.
//!
//! A backup that fails does not stop the server: what clients wrote is
//! in the log all the same. It says so once, tries again each time, and
//! says when it works again. A place that held the backup and no longer
//! does, as when its disk is gone, is not started over: the same path
//! would be another disk.

//!
//! `UMBRA.BACKUP` says to any client how that goes: how many records the
//! backup holds of the store's, how long ago the last one finished, and
//! whether backups fail. It does not say where the backup is kept, nor
//! why one failed: the reason can name the place, and is said to
//! whoever runs the server.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use std::{io, thread};

use serde_json::json;

use super::resp::Reply;
use crate::store::Writer;
use crate::store::backup::{Backup, BackupError, DirTarget, Origin, back_up};

const BACKUP: &str = "UMBRA.BACKUP";

/// What a server that keeps a backup knows of how that goes.
#[derive(Debug, Default)]
pub(super) struct Kept {
    /// How many records the backup holds, once one has finished.
    held: Option<u64>,
    /// When the last backup that copied anything finished.
    finished: Option<Instant>,
    is_failing: bool,
}

pub(super) type Shared = Arc<Mutex<Kept>>;

/// The reply to the backup command a client may send, or `None` if
/// `name` is not it. `kept` is `None` on a server that keeps no backup.
pub(super) fn reply(
    writer: &Writer,
    kept: Option<&Shared>,
    name: &str,
    args: &[Vec<u8>],
) -> Option<Reply> {
    if name != BACKUP {
        return None;
    }
    if !args.is_empty() {
        return Some(Reply::Error(format!(
            "ERR wrong number of arguments for '{}' command",
            BACKUP.to_ascii_lowercase()
        )));
    }
    let status = match kept {
        None => json!({ "keeps_a_backup": false }),
        Some(kept) => {
            let kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
            json!({
                "keeps_a_backup": true,
                "records": kept.held,
                "store_records": writer.held_records(),
                "finished_seconds_ago": kept.finished.map(|at| at.elapsed().as_secs()),
                "failing": kept.is_failing,
            })
        }
    };
    Some(Reply::Bulk(Some(status.to_string().into_bytes())))
}

/// Starts the thread that brings the backup in `dir` up to date, now
/// and then every `every`, and returns where it says how that goes.
pub(super) fn spawn(writer: Writer, dir: PathBuf, every: Duration) -> io::Result<Shared> {
    let kept = Shared::default();
    let told = Arc::clone(&kept);
    thread::Builder::new()
        .name("backups".to_owned())
        .spawn(move || {
            let mut backups = Backups::new(writer, dir);
            loop {
                if let Some(said) = backups.run() {
                    eprintln!("umbrachyx: {said}");
                }
                *told.lock().unwrap_or_else(PoisonError::into_inner) = Kept {
                    held: backups.held,
                    finished: backups.finished,
                    is_failing: backups.failure.is_some(),
                };
                thread::sleep(every);
            }
        })
        .map(|_| kept)
}

struct Backups {
    writer: Writer,
    dir: PathBuf,
    /// How many records the backup holds, once one has finished.
    held: Option<u64>,
    /// When the last backup that copied anything finished.
    finished: Option<Instant>,
    /// Why the last backup failed, if it did.
    failure: Option<String>,
}

impl Backups {
    fn new(writer: Writer, dir: PathBuf) -> Self {
        Self {
            writer,
            dir,
            held: None,
            finished: None,
            failure: None,
        }
    }

    /// Takes a backup if anything was written since the last, and
    /// returns what to tell whoever runs the server: that backups fail,
    /// with each new reason, and that they work again.
    fn run(&mut self) -> Option<String> {
        match self.back_up() {
            Ok(None) => None,
            Ok(Some(backup)) => {
                self.held = Some(backup.records);
                self.finished = Some(Instant::now());
                self.failure.take().map(|_| {
                    format!(
                        "the backup to {} works again: it holds {} records",
                        self.dir.display(),
                        backup.records
                    )
                })
            }
            Err(error) => {
                let reason = error.to_string();
                if self.failure.as_ref() == Some(&reason) {
                    return None;
                }
                let said = format!(
                    "the backup to {} failed: {reason}. The server goes on, and tries again",
                    self.dir.display()
                );
                self.failure = Some(reason);
                Some(said)
            }
        }
    }

    /// `None` when the backup already holds every record: neither the
    /// place nor more of the store than a count is touched.
    fn back_up(&mut self) -> Result<Option<Backup>, BackupError> {
        if self.failure.is_none() && Some(self.writer.records()?) == self.held {
            return Ok(None);
        }
        // Opened for one backup and let go, so the place can be copied
        // or looked at between two. Once a backup is there the place is
        // not made again: one that has gone is said to have.
        let mut target = match self.held {
            Some(_) => DirTarget::open_existing(&self.dir)?,
            None => DirTarget::open(&self.dir)?,
        };
        back_up(&mut self.writer, &mut target).map(Some)
    }
}
