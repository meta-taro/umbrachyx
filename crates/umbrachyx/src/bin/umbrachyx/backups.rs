//! Where `serve` keeps a backup of the store, and how often it brings it
//! up to date. And `backup --check`, which asks a backup whether it
//! would restore, and `backup --status`, which asks a running server how
//! its backup is going.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use umbrachyx::store::backup::{self, BackupError, DirSource, DirTarget};

use crate::{Args, FileStore, replication};

/// How often a served store is backed up unless told otherwise.
const DEFAULT_EVERY: Duration = Duration::from_secs(60);

/// The place `--backup-to` names and the time `--backup-every` gives,
/// or `None` without them. The place is checked before the server
/// listens: a backup that could never be kept there is found now, not
/// by reading what the server says later.
pub fn place(store: &mut FileStore, args: &Args) -> Result<Option<(PathBuf, Duration)>, String> {
    let every = match args.options.get("backup-every") {
        Some(seconds) => seconds
            .parse()
            .ok()
            .filter(|&seconds: &f64| seconds > 0.0)
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .ok_or_else(|| format!("--backup-every {seconds:?} is not a number of seconds"))?,
        None => DEFAULT_EVERY,
    };
    let Some(dir) = args.options.get("backup-to") else {
        if args.options.contains_key("backup-every") {
            return Err("--backup-every needs --backup-to, the directory to back up to".to_owned());
        }
        return Ok(None);
    };
    let mut target = DirTarget::open(dir.as_ref()).map_err(|e| format!("{dir}: {e}"))?;
    backup::check(store, &mut target).map_err(|e| format!("{dir}: {e}"))?;
    eprintln!("backing up to {dir} every {} s", every.as_secs_f64());
    Ok(Some((PathBuf::from(dir), every)))
}

/// `backup <directory> --check`: reads the backup in the directory and
/// says whether it would restore. Nothing is written, the directory
/// included: a place with no backup is not made into one.
pub fn verify(args: &Args) -> Result<ExitCode, String> {
    let [dir] = args.exactly()?;
    let checked =
        backup::verify(&mut DirSource::new(dir.as_ref())).map_err(|e| refused(dir, &e))?;
    let keys = if checked.has_keys {
        "its key file"
    } else {
        "no key file"
    };
    println!(
        "the backup in {dir} would restore: {} records ({} bytes of log) and {keys}, as it says",
        checked.records, checked.bytes
    );
    Ok(ExitCode::SUCCESS)
}

/// `UMBRA.BACKUP`, as the protocol sends a command.
const ASK: &[u8] = b"*1\r\n$12\r\nUMBRA.BACKUP\r\n";

/// `backup --status`: asks the server on `--host` and `--port` how its
/// backup is going and prints the answer a line per fact. It opens no
/// log. The server does not say where the backup is kept nor why one
/// failed, so neither is printed: the reason is on its standard error.
pub fn status(args: &Args, default_port: u16) -> Result<ExitCode, String> {
    let [] = args.exactly()?;
    let (host, port) = replication::server(args, default_port)?;
    let answered =
        replication::ask(&host, port, ASK).map_err(|why| format!("{host}:{port}: {why}"))?;
    let not_a_status = || format!("{host}:{port}: {}", replication::NOT_A_STATUS);
    if !answered["keeps_a_backup"]
        .as_bool()
        .ok_or_else(not_a_status)?
    {
        println!("keeps a backup: no");
        return Ok(ExitCode::SUCCESS);
    }
    let is_failing = answered["failing"].as_bool().ok_or_else(not_a_status)?;
    let store = answered["store_records"]
        .as_u64()
        .ok_or_else(not_a_status)?;
    println!("keeps a backup: yes");
    match answered["records"].as_u64() {
        Some(held) => println!("records: {held} of the store's {store}"),
        None => println!("records: none yet of the store's {store}"),
    }
    match answered["finished_seconds_ago"].as_u64() {
        Some(seconds) => println!("last finished: {seconds} s ago"),
        None => println!("last finished: not since the server started"),
    }
    if is_failing {
        println!("failing: yes; the server says why on its standard error");
        return Ok(ExitCode::from(replication::NOT_WELL));
    }
    println!("failing: no");
    Ok(ExitCode::SUCCESS)
}

/// What Windows answers a read of a part of a file another process has
/// locked.
const LOCKED_ON_WINDOWS: i32 = 33;

/// Why the backup in `dir` could not be read, as it is said to whoever
/// asked. A server that keeps its backup there holds the copy of the log
/// while it writes one, and on Windows nobody else can read it for that
/// time: the backup is not damaged, and asking again works.
pub fn refused(dir: &str, error: &BackupError) -> String {
    let is_being_written = cfg!(windows)
        && matches!(error, BackupError::Io(error) if error.raw_os_error() == Some(LOCKED_ON_WINDOWS));
    if is_being_written {
        format!(
            "{dir}: {error}\na server may be writing a backup there, and holds it while it does: \
             nothing is wrong with the backup, run this again"
        )
    } else {
        format!("{dir}: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_locked_on_windows_is_said_to_be_one_to_ask_again() {
        let locked = BackupError::Io(std::io::Error::from_raw_os_error(LOCKED_ON_WINDOWS));

        let said = refused("the-backup", &locked);

        assert!(said.starts_with("the-backup: "), "{said}");
        // Elsewhere that number is another error, and nothing is added.
        assert_eq!(said.contains("run this again"), cfg!(windows), "{said}");
    }

    #[test]
    fn a_damaged_backup_is_not_said_to_be_one_to_ask_again() {
        let damaged = BackupError::Damaged("the key file is missing");

        let said = refused("the-backup", &damaged);

        assert!(!said.contains("run this again"), "{said}");
    }
}
