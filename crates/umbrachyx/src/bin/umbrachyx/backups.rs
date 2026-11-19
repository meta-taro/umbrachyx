//! Where `serve` keeps a backup of the store, and how often it brings it
//! up to date.

use std::path::PathBuf;
use std::time::Duration;

use umbrachyx::store::backup::{self, DirTarget};

use crate::{Args, FileStore};

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
