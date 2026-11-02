//! Command-line access to a store: write commits, read values and their
//! history, branch, diff, merge, and undo a commit.
//!
//! The log file is named by `UMBRACHYX_DB`. Exit status is 0 on success, 1
//! when a merge or undo stopped on conflicts, and 2 on any other error.

use std::collections::HashMap;
use std::process::ExitCode;

use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::store::{
    CommitId, Conflict, MAIN_BRANCH, Merged, Provenance, Reverted, Store, SystemClock, short,
};
use umbrachyx::tree::Bytes;

const DB_ENV: &str = "UMBRACHYX_DB";
const DEFAULT_DB: &str = "umbrachyx.log";
const USAGE: &str = "\
usage: umbrachyx <command> ...   (log file: $UMBRACHYX_DB, default umbrachyx.log)

  commit <branch> [key=value]... [--remove key]... --who W --why Y [--approved-by A]
  get <branch> <key>
  history <branch> <key>
  log <branch> [--limit N]
  branch <name> --from <branch>
  diff <from> <to>
  merge <into> <from> --who W --why Y [--approved-by A]
  revert <branch> <commit> --who W --why Y [--approved-by A]

A commit is named by a branch (its latest commit), <branch>~N (N back on
its own line), or the start of its id.";

type FileStore = Store<FileStorage, SystemClock>;

/// Stopped on conflicts, as opposed to failing.
const CONFLICTS: u8 = 1;
const FAILED: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("umbrachyx: {error}");
            ExitCode::from(FAILED)
        }
    }
}

/// Positional arguments and `--name value` options, in order.
struct Args {
    positional: Vec<String>,
    options: HashMap<String, String>,
    removes: Vec<String>,
}

impl Args {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut parsed = Self {
            positional: Vec::new(),
            options: HashMap::new(),
            removes: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let Some(name) = arg.strip_prefix("--") else {
                parsed.positional.push(arg.clone());
                continue;
            };
            let value = args
                .next()
                .ok_or_else(|| format!("--{name} needs a value"))?
                .clone();
            match name {
                "remove" => parsed.removes.push(value),
                "who" | "why" | "approved-by" | "from" | "limit" => {
                    parsed.options.insert(name.to_owned(), value);
                }
                _ => return Err(format!("unknown option --{name}\n\n{USAGE}")),
            }
        }
        Ok(parsed)
    }

    /// Exactly `N` positional arguments after the command.
    fn exactly<const N: usize>(&self) -> Result<[&str; N], String> {
        let rest: Vec<&str> = self.positional[1..].iter().map(String::as_str).collect();
        rest.try_into()
            .map_err(|_| format!("{} takes {N} arguments\n\n{USAGE}", self.positional[0]))
    }

    fn option(&self, name: &str) -> Result<&str, String> {
        self.options
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn provenance(&self) -> Result<Provenance, String> {
        Ok(Provenance {
            who: self.option("who")?.to_owned(),
            why: self.option("why")?.to_owned(),
            approved_by: self.options.get("approved-by").cloned(),
        })
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let args = Args::parse(args)?;
    let Some(command) = args.positional.first() else {
        return Err(USAGE.to_owned());
    };
    let path = std::env::var(DB_ENV).unwrap_or_else(|_| DEFAULT_DB.to_owned());
    let storage = FileStorage::open(path.as_ref()).map_err(|e| format!("{path}: {e}"))?;
    let mut store = Store::open(storage, SystemClock).map_err(|e| format!("{path}: {e}"))?;
    match command.as_str() {
        "commit" => commit(&mut store, &args),
        "get" => get(&store, &args),
        "history" => history(&store, &args),
        "log" => log(&store, &args),
        "branch" => branch(&mut store, &args),
        "diff" => diff(&store, &args),
        "merge" => merge(&mut store, &args),
        "revert" => revert(&mut store, &args),
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

fn commit(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let (branch, sets) = args.positional[1..]
        .split_first()
        .ok_or_else(|| format!("commit needs a branch\n\n{USAGE}"))?;
    let mut edits = Vec::new();
    for set in sets {
        let (key, value) = set
            .split_once('=')
            .ok_or_else(|| format!("{set:?} is not key=value"))?;
        edits.push(Edit {
            key: key.as_bytes().to_vec(),
            value: Some(value.as_bytes().to_vec()),
        });
    }
    edits.extend(args.removes.iter().map(|key| Edit {
        key: key.as_bytes().to_vec(),
        value: None,
    }));
    if edits.is_empty() {
        return Err("nothing to commit".to_owned());
    }
    let id = store
        .commit(branch, edits, args.provenance()?)
        .map_err(|e| e.to_string())?;
    println!("committed {} to {branch}", short(&id));
    Ok(ExitCode::SUCCESS)
}

fn get(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch, key] = args.exactly()?;
    require_branch(store, branch)?;
    println!("{}", shown(store.get(branch, key.as_bytes()).as_ref()));
    Ok(ExitCode::SUCCESS)
}

fn history(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch, key] = args.exactly()?;
    require_branch(store, branch)?;
    for cell in store.history(branch, key.as_bytes()) {
        let info = store
            .commit_info(&cell.changed_by)
            .ok_or("a cell names an unknown commit")?;
        let value = cell
            .value
            .as_ref()
            .map_or_else(|| "removed".to_owned(), |v| shown(Some(v)));
        println!(
            "{}  {}  {value}  by {}  {:?}{}",
            utc(info.time),
            short(&info.id),
            info.provenance.who,
            info.provenance.why,
            approval(&info.provenance)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn log(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch] = args.exactly()?;
    require_branch(store, branch)?;
    let limit = match args.options.get("limit") {
        Some(n) => n
            .parse()
            .map_err(|_| format!("--limit {n:?} is not a number"))?,
        None => usize::MAX,
    };
    for info in store.log(branch).into_iter().take(limit) {
        println!(
            "{}  {}  {}  {:?}{}",
            short(&info.id),
            utc(info.time),
            info.provenance.who,
            info.provenance.why,
            approval(&info.provenance)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn branch(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let [name] = args.exactly()?;
    let from = args.option("from")?;
    store.create_branch(name, from).map_err(|e| e.to_string())?;
    println!("created {name} from {from}");
    Ok(ExitCode::SUCCESS)
}

fn diff(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [from, to] = args.exactly()?;
    let changes = store
        .diff(point(store, from)?.as_ref(), point(store, to)?.as_ref())
        .map_err(|e| e.to_string())?;
    for change in changes {
        let key = String::from_utf8_lossy(&change.key);
        match (&change.before, &change.after) {
            (None, after) => println!("+ {key}: {}", shown(after.as_ref())),
            (before, None) => println!("- {key}: {}", shown(before.as_ref())),
            (before, after) => println!(
                "~ {key}: {} -> {}",
                shown(before.as_ref()),
                shown(after.as_ref())
            ),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn merge(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let [into, from] = args.exactly()?;
    let merged = store
        .merge(into, from, args.provenance()?)
        .map_err(|e| e.to_string())?;
    match merged {
        Merged::Commit(id) => println!("merged {}", short(&id)),
        Merged::UpToDate => println!("up to date"),
        Merged::Conflicts(conflicts) => return Ok(report(&conflicts)),
    }
    Ok(ExitCode::SUCCESS)
}

fn revert(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch, name] = args.exactly()?;
    let commit = store.resolve(name).map_err(|e| e.to_string())?;
    let reverted = store
        .revert(branch, &commit, args.provenance()?)
        .map_err(|e| e.to_string())?;
    match reverted {
        Reverted::Commit(id) => println!("reverted with {}", short(&id)),
        Reverted::Conflicts(conflicts) => return Ok(report(&conflicts)),
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints each conflict; nothing was written.
fn report(conflicts: &[Conflict]) -> ExitCode {
    for conflict in conflicts {
        println!(
            "conflict on {}: base {}, ours {}, theirs {}",
            String::from_utf8_lossy(&conflict.key),
            shown(conflict.base.as_ref()),
            shown(conflict.ours.as_ref()),
            shown(conflict.theirs.as_ref())
        );
    }
    println!("nothing was written");
    ExitCode::from(CONFLICTS)
}

/// A branch with no commits yet stands for the empty state.
fn point(store: &FileStore, name: &str) -> Result<Option<CommitId>, String> {
    if store.branches().any(|branch| branch == name) {
        return Ok(store.head(name));
    }
    store.resolve(name).map(Some).map_err(|e| e.to_string())
}

fn require_branch(store: &FileStore, name: &str) -> Result<(), String> {
    if store.branches().any(|branch| branch == name) {
        Ok(())
    } else {
        Err(format!("no branch named {name:?} (try {MAIN_BRANCH:?})"))
    }
}

fn shown(value: Option<&Bytes>) -> String {
    value.map_or_else(
        || "(absent)".to_owned(),
        |v| String::from_utf8_lossy(v).into_owned(),
    )
}

fn approval(provenance: &Provenance) -> String {
    provenance
        .approved_by
        .as_ref()
        .map_or_else(String::new, |by| format!("  approved by {by}"))
}

/// Microseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc(micros: u64) -> String {
    let seconds = micros / 1_000_000;
    let (days, of_day) = (seconds / 86_400, seconds % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

/// Days since 1970-01-01 to a Gregorian date, after Howard Hinnant's
/// `civil_from_days`, for dates from 1970 on.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_print_as_utc() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400_000_000), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_793_621_045_123_456), "2026-11-02T12:04:05Z");
    }
}
