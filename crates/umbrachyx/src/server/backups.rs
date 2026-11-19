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

use std::path::PathBuf;
use std::time::Duration;
use std::{io, thread};

use crate::store::Writer;
use crate::store::backup::{Backup, BackupError, DirTarget, Origin, back_up};

/// Starts the thread that brings the backup in `dir` up to date, now
/// and then every `every`.
pub(super) fn spawn(writer: Writer, dir: PathBuf, every: Duration) -> io::Result<()> {
    thread::Builder::new()
        .name("backups".to_owned())
        .spawn(move || {
            let mut backups = Backups::new(writer, dir);
            loop {
                if let Some(said) = backups.run() {
                    eprintln!("umbrachyx: {said}");
                }
                thread::sleep(every);
            }
        })
        .map(drop)
}

struct Backups {
    writer: Writer,
    dir: PathBuf,
    /// How many records the backup holds, once one has finished.
    held: Option<u64>,
    /// Why the last backup failed, if it did.
    failure: Option<String>,
}

impl Backups {
    fn new(writer: Writer, dir: PathBuf) -> Self {
        Self {
            writer,
            dir,
            held: None,
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
