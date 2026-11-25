//! Command-line access to a store: write commits, read values and their
//! history, branch, diff, merge, undo a commit, erase a subject, declare how
//! a table is held, write the log out as JSON Lines, back the store up and
//! restore it, and serve a branch over the Redis protocol. Also makes the root key and the capability tokens
//! that agents carry.
//!
//! The log file is named by `UMBRACHYX_DB`. Values are encrypted under a key
//! per subject, kept in the file next to it with `.keys` added to its name.
//! Exit status is 0 on success, 1 when a merge or undo stopped on
//! conflicts or when `replication` found a server whose replication is
//! not going well, and 2 on any other error.

use std::collections::HashMap;
use std::io::{BufWriter, ErrorKind, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::process::ExitCode;
use std::time::Duration;

use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::server::{DEFAULT_SYNC_EVERY, MAX_CONNECTIONS, Server};
use umbrachyx::store::backup::{self, DirSource, DirTarget};
use umbrachyx::store::{
    CommitId, Committer, Conflict, MAIN_BRANCH, Merged, Provenance, Reverted, Store, SystemClock,
    short,
};
use umbrachyx::tree::Bytes;

mod allocator;
mod backups;
mod replication;
mod tokens;

const DB_ENV: &str = "UMBRACHYX_DB";
const DEFAULT_DB: &str = "umbrachyx.log";
/// One past Redis's own, so both can run on one machine.
const DEFAULT_PORT: u16 = 6380;
const USAGE: &str = "\
usage: umbrachyx <command> ...   (log file: $UMBRACHYX_DB, default umbrachyx.log;
                                  its keys: the same name with .keys added)

  commit <branch> [key=value]... [--remove key]... --who W --why Y [--approved-by A]
  put [key=value]... [--remove key]...
                              writes tables that keep no history (plain):
                              the value is replaced on every branch at once
  get <branch> <key>
  history <branch> <key>
  log <branch> [--limit N]
  branch <name> --from <branch>
  diff <from> <to>
  merge <into> <from> --who W --why Y [--approved-by A]
  revert <branch> <commit> --who W --why Y [--approved-by A]
  erase <subject> --who W --why Y [--approved-by A]
                              destroys the key of a subject such as player/42
  erasures                    every subject erased, with who and why
  table <name> <history|latest|plain|plain-async> --who W --why Y
        [--approved-by A]
                              how the table <name>/... is held. latest keeps
                              only current values of what commits write.
                              plain keeps no history at all: no commits, no
                              branches, no review, written with put or SET.
                              plain-async also answers a SET before it is on
                              disk. Only before the table's first write
  tables                      every declared table and its engine
  export                      every record of the log as a line of JSON, for
                              jq and grep: commits with their changes,
                              branches, tables, rejections and erasures
  backup <directory>          brings the backup in <directory> up to what the
                              store holds now: its log and its keys as they
                              are at one moment. The first run copies
                              everything, later runs what was written since,
                              and a subject erased since has no key in the
                              backup afterwards. Not while a server has the
                              log open: serve --backup-to does it then
  restore <directory>         writes the store the backup in <directory>
                              holds to $UMBRACHYX_DB, which must not exist yet
  serve <branch> [--port P] [--bind A] [--max-connections N] [--threads T]
        [--sync-every MS] [--public-key K [--require-auth]]
        [--backup-to D [--backup-every S]] [--feed-replicas-on P [--wait-for-replica S]] [--follow H:P]
        [--replication-cert F --replication-key F --replication-ca F]
        [--current-values-only]
                              Redis protocol on 127.0.0.1: GET, SET, PING,
                              SCAN, TYPE, TTL, and UMBRA.* to read history.
                              --bind listens on another address of this
                              machine, such as 0.0.0.0 inside a container;
                              reads need no token and nothing is encrypted,
                              so only where the network itself is trusted.
                              With a public key, AUTH <token> names who
                              writes; --require-auth refuses writes without it.
                              --threads is how many threads serve connections
                              (one more writes); by default one for every two
                              processors, four at most.
                              --sync-every is the longest a SET to a
                              plain-async table is answered before it is on
                              disk, in milliseconds (default 1000): what a
                              power cut can lose of such a table. Every other
                              SET is on disk before its answer.
                              --backup-to keeps a backup in a directory, as
                              `backup` does, brought up to date when the
                              server starts and every --backup-every seconds
                              (default 60) while it serves. No client can
                              ask for a backup or say where one goes.
                              --feed-replicas-on hands the records of the
                              log, every branch of it, to whoever asks on
                              port P of the address it listens on: open it
                              to its replicas only.
                              --wait-for-replica answers a write only once
                              a replica has said it holds it, waiting S
                              seconds at most. A write no replica held in
                              time is answered NOTREPLICATED: it is in this
                              server's log. The writes after it are
                              answered NOREPLICA and not written, until a
                              replica holds everything. SETs to plain-async
                              tables do not wait.
                              --follow makes this server a replica of the
                              one that hands its log out at H:P: it asks
                              that server for its
                              records, answers reads, and refuses writes
                              with READONLY. Its store is a backup of the
                              other, made with restore.
                              --replication-cert, --replication-key and
                              --replication-ca name three PEM files: this
                              server's certificate, the private key of it,
                              and the certificate of the authority that
                              signed the other servers'. With them the
                              servers speak TLS 1.3 to each other, a
                              primary answers only replicas that show a
                              certificate that authority signed, a replica
                              asks only a primary that shows one for the
                              host in --follow (an address there needs the
                              address in the certificate), and a replica
                              is sent the keys of subjects first written
                              after its backup. Give them to both servers.
                              Whoever holds a certificate that authority
                              signed is sent every record and every key:
                              use an authority that signs nothing else.
                              Without them nothing between the servers is
                              encrypted and keys are not sent: values of a
                              subject first written after the backup stop
                              the replica, and it says so. The port
                              clients use is not encrypted either way.
                              --current-values-only, with --follow, keeps
                              in memory the values as they are now and
                              nothing of the past: what the replica holds
                              no longer grows with every write. It answers
                              GET and SCAN as before, and every question
                              about the past (UMBRA.HISTORY, UMBRA.LOG,
                              UMBRA.DIFF, UMBRA.CELLS, UMBRA.PENDING,
                              UMBRA.DECISIONS) with NOHISTORY. A SCAN there
                              walks in no order, and may return a key
                              twice or miss one if writes arrive meanwhile.
                              Its log on disk is whole: started without
                              the option, it answers those too. Starting
                              reads the whole log, as any server's does
  mcp --public-key K --token-file F
                              MCP over stdin and stdout, for one agent's token

  replication [--host A] [--port P]
                              asks the server running on this machine, or
                              on A, how its replication is going: primary
                              or replica, what it holds, how many records
                              it and the other end have, and why a replica
                              stopped or cannot reach its primary. Opens no
                              log. Exit status 1 if a replica stopped or
                              cannot reach its primary, or a primary refuses
                              writes because no replica keeps up
  keygen <file>               a new root key into <file>; prints its public key
  token issue --key <file> --subject S --branch B --prefix P --columns C,..
              --operations read,write --purpose U --expires-in 8h
                              a person's token: --operations read,approve and
                              no --branch; a token never both writes and approves
  token narrow --public-key K [--operation O] [--prefix P] [--column C]
               [--expires-in D]       reads a token on stdin, prints it narrowed
  token show --public-key K   reads a token on stdin, prints who holds it

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
/// How far ahead of its records a server zeroes its log, at a time.
const LOG_RESERVE: u64 = 16 * 1024 * 1024;

/// Options that take no value.
const FLAGS: &[&str] = &["require-auth", "current-values-only"];

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
            if FLAGS.contains(&name) {
                parsed.options.insert(name.to_owned(), String::new());
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("--{name} needs a value"))?
                .clone();
            match name {
                "remove" => parsed.removes.push(value),
                "who" | "why" | "approved-by" | "from" | "limit" | "port" | "bind" | "host"
                | "max-connections" | "threads" | "sync-every" | "key" | "subject" | "branch"
                | "prefix" | "columns" | "column" | "operations" | "operation" | "purpose"
                | "expires-in" | "public-key" | "token-file" | "backup-to" | "backup-every"
                | "follow" | "feed-replicas-on" | "wait-for-replica" | "replication-cert"
                | "replication-key" | "replication-ca" => {
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
    // These never touch a log.
    match command.as_str() {
        "keygen" => return tokens::keygen(&args),
        "token" => return tokens::token(&args),
        "replication" => return replication::status(&args, DEFAULT_PORT),
        _ => {}
    }
    let path = std::env::var(DB_ENV).unwrap_or_else(|_| DEFAULT_DB.to_owned());
    if command == "restore" {
        return restore(&path, &args);
    }
    // A server syncs for every write, so its log is zeroed ahead of what it
    // writes and a sync has only the record to make durable. Other commands
    // write once and leave; they open the log as it is and give back any
    // zeros a server left at its end.
    let reserve = if command == "serve" { LOG_RESERVE } else { 0 };
    let storage =
        FileStorage::open_reserving(path.as_ref(), reserve).map_err(|e| format!("{path}: {e}"))?;
    let keys_path = format!("{path}.keys");
    let key_storage: Box<dyn KeyStorage + Send> = Box::new(FileKeyStorage::new(keys_path.as_ref()));
    let keys = Keys::open(key_storage).map_err(|e| format!("{keys_path}: {e}"))?;
    if command == "export" {
        return export(storage, keys, &args).map_err(|e| format!("{path}: {e}"));
    }
    let opened = if replication::holds_current_values_only(command, &args)? {
        Store::open_current_values(storage, Some(keys), SystemClock)
    } else {
        Store::open_sealed(storage, keys, SystemClock)
    };
    let mut store = opened.map_err(|e| format!("{path}: {e}"))?;
    match command.as_str() {
        "commit" => commit(&mut store, &args),
        "put" => put(&mut store, &args),
        "get" => get(&store, &args),
        "history" => history(&store, &args),
        "log" => log(&store, &args),
        "branch" => branch(&mut store, &args),
        "diff" => diff(&store, &args),
        "merge" => merge(&mut store, &args),
        "revert" => revert(&mut store, &args),
        "erase" => erase(&mut store, &args),
        "erasures" => erasures(&store, &args),
        "table" => table(&mut store, &args),
        "tables" => tables(&store, &args),
        "backup" => back_up(&mut store, &args),
        "serve" => serve(store, &args),
        "mcp" => tokens::mcp(store, &args),
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
    }
}

/// Prints every record of the log, a line of JSON each. A reader that
/// stops early, such as `head`, is not an error.
fn export(
    storage: FileStorage,
    keys: Keys<Box<dyn KeyStorage + Send>>,
    args: &Args,
) -> Result<ExitCode, String> {
    let [] = args.exactly()?;
    let lines = FileStore::export(storage, keys, SystemClock).map_err(|e| e.to_string())?;
    let mut out = BufWriter::new(std::io::stdout().lock());
    let written = lines
        .iter()
        .try_for_each(|line| writeln!(out, "{line}"))
        .and_then(|()| out.flush());
    match written {
        Err(error) if error.kind() != ErrorKind::BrokenPipe => Err(error.to_string()),
        _ => Ok(ExitCode::SUCCESS),
    }
}

fn back_up(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let [dir] = args.exactly()?;
    let mut target = DirTarget::open(dir.as_ref()).map_err(|e| format!("{dir}: {e}"))?;
    let done = store
        .backup(&mut target)
        .map_err(|e| format!("{dir}: {e}"))?;
    println!(
        "backed up {} records to {dir} ({} new, {} bytes)",
        done.records, done.added, done.bytes
    );
    Ok(ExitCode::SUCCESS)
}

/// Writes the store a backup holds to `path`, where there must be none.
/// The files this made are removed again if the backup is refused, so a
/// failed restore leaves no empty store behind.
fn restore(path: &str, args: &Args) -> Result<ExitCode, String> {
    let [dir] = args.exactly()?;
    let keys_path = format!("{path}.keys");
    for existing in [path, keys_path.as_str()] {
        if std::path::Path::new(existing).exists() {
            return Err(format!(
                "{existing} exists; restore to a place where there is no store"
            ));
        }
    }
    let mut target = DirSource::new(dir.as_ref());
    let mut log = FileStorage::open(path.as_ref()).map_err(|e| format!("{path}: {e}"))?;
    let mut keys = FileKeyStorage::new(keys_path.as_ref());
    match backup::restore(&mut target, &mut log, Some(&mut keys)) {
        Ok(restored) => {
            println!("restored {} records from {dir} to {path}", restored.records);
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            drop(log);
            // Whatever is left is ours: neither file was there before.
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(&keys_path);
            Err(format!("{dir}: {error}"))
        }
    }
}

fn commit(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let (branch, sets) = args.positional[1..]
        .split_first()
        .ok_or_else(|| format!("commit needs a branch\n\n{USAGE}"))?;
    let edits = edits(sets, &args.removes)?;
    if edits.is_empty() {
        return Err("nothing to commit".to_owned());
    }
    let id = store
        .commit(branch, edits, args.provenance()?)
        .map_err(|e| e.to_string())?;
    println!("committed {} to {branch}", short(&id));
    Ok(ExitCode::SUCCESS)
}

/// Each `key=value` as a write, then each key to remove.
fn edits(sets: &[String], removes: &[String]) -> Result<Vec<Edit>, String> {
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
    edits.extend(removes.iter().map(|key| Edit {
        key: key.as_bytes().to_vec(),
        value: None,
    }));
    Ok(edits)
}

/// Writes tables that keep no history: no branch, and no one to name,
/// since nothing would record it.
fn put(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let edits = edits(&args.positional[1..], &args.removes)?;
    if edits.is_empty() {
        return Err("nothing to put".to_owned());
    }
    let count = edits.len();
    store.put(edits).map_err(|e| e.to_string())?;
    println!("put {count} key{}", if count == 1 { "" } else { "s" });
    Ok(ExitCode::SUCCESS)
}

fn get(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch, key] = args.exactly()?;
    require_branch(store, branch)?;
    let (value, erased) = store.current(branch, key.as_bytes());
    if erased {
        println!("(erased)");
    } else {
        println!("{}", shown(value.as_ref()));
    }
    Ok(ExitCode::SUCCESS)
}

fn history(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch, key] = args.exactly()?;
    require_branch(store, branch)?;
    if let Some(table) = store.table_without_history_of(key.as_bytes()) {
        return Err(format!(
            "table '{}' keeps only the latest value; there is no history to show",
            String::from_utf8_lossy(table)
        ));
    }
    for cell in store.history(branch, key.as_bytes()) {
        let info = store
            .commit_info(&cell.changed_by)
            .ok_or("a cell names an unknown commit")?;
        let value = match &cell.value {
            _ if cell.erased => "erased".to_owned(),
            None => "removed".to_owned(),
            Some(value) => shown(Some(value)),
        };
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

/// Destroys a subject's key: its values, current and past, cannot be read
/// again, and its history keeps its shape.
fn erase(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let [subject] = args.exactly()?;
    let erased = store
        .erase(subject.as_bytes(), args.provenance()?)
        .map_err(|e| e.to_string())?;
    if erased {
        println!("erased {subject}");
    } else {
        println!("{subject} has no key; nothing was erased");
    }
    Ok(ExitCode::SUCCESS)
}

fn erasures(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [] = args.exactly()?;
    for erasure in store.erasures() {
        println!(
            "{}  {}  by {}  {:?}{}",
            utc(erasure.time),
            String::from_utf8_lossy(&erasure.subject),
            erasure.provenance.who,
            erasure.provenance.why,
            approval(&erasure.provenance)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Declares the engine of a table that nothing has been written to yet.
fn table(store: &mut FileStore, args: &Args) -> Result<ExitCode, String> {
    let [name, engine_name] = args.exactly()?;
    let engine = Engine::named(engine_name).ok_or_else(|| {
        format!("engine {engine_name:?} is not one of history, latest, plain or plain-async")
    })?;
    store
        .declare_table(name.as_bytes(), engine, args.provenance()?)
        .map_err(|e| e.to_string())?;
    println!("declared {name} {}", engine.name());
    Ok(ExitCode::SUCCESS)
}

fn tables(store: &FileStore, args: &Args) -> Result<ExitCode, String> {
    let [] = args.exactly()?;
    for (name, engine) in store.tables() {
        println!("{}  {}", String::from_utf8_lossy(name), engine.name());
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
    let (from, to) = (point(store, from)?, point(store, to)?);
    let changes = store
        .diff(from.as_ref(), to.as_ref())
        .map_err(|e| e.to_string())?;
    for change in changes {
        let key = String::from_utf8_lossy(&change.key);
        if change.erased {
            println!("~ {key}: erased");
            continue;
        }
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
    // On stderr, so a script reading the changes reads only changes.
    let engines = store.tables();
    for table in store.omitted_tables(from.as_ref(), to.as_ref()) {
        let is_plain = engines.get(&table).is_some_and(|engine| engine.is_plain());
        let why = if is_plain {
            "keeps no history, so no commit holds its writes"
        } else {
            "keeps only its latest values, and main keeps no past for it"
        };
        eprintln!("(left out: {} {why})", String::from_utf8_lossy(&table));
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

/// What `serve` says when told to listen on an address other machines
/// can reach.
const BEYOND_THIS_MACHINE: &str = "listening beyond this machine: anyone who can reach the port \
    reads every value and its history without a token, and nothing is encrypted";

/// What `serve` says when it feeds replicas or follows a server with no
/// certificates given.
const REPLICATION_IN_THE_CLEAR: &str = "nothing between this server and its replicas or \
    primary is encrypted, and keys are not sent: give --replication-cert, --replication-key \
    and --replication-ca to change that";

/// Where `serve` listens: this machine only, unless `--bind` names another
/// of its addresses.
fn listen_address(args: &Args) -> Result<IpAddr, String> {
    match args.options.get("bind") {
        Some(address) => address
            .parse()
            .map_err(|_| format!("--bind {address:?} is not an address")),
        None => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    }
}

/// Serves one branch until stopped, only on this machine unless `--bind`
/// says otherwise. With a public
/// key, connections may `AUTH` with a capability token; with
/// `--require-auth`, only those may write.
fn serve(mut store: FileStore, args: &Args) -> Result<ExitCode, String> {
    let [branch] = args.exactly()?;
    let port: u16 = match args.options.get("port") {
        Some(port) => port
            .parse()
            .map_err(|_| format!("--port {port:?} is not a port"))?,
        None => DEFAULT_PORT,
    };
    let address = listen_address(args)?;
    let max_connections = match args.options.get("max-connections") {
        Some(max) => max
            .parse()
            .ok()
            .filter(|&max: &usize| max > 0)
            .ok_or_else(|| format!("--max-connections {max:?} is not a positive number"))?,
        None => MAX_CONNECTIONS,
    };
    let threads = match args.options.get("threads") {
        Some(threads) => Some(
            threads
                .parse()
                .ok()
                .filter(|&threads: &usize| threads > 0)
                .ok_or_else(|| format!("--threads {threads:?} is not a positive number"))?,
        ),
        None => None,
    };
    let sync_every = match args.options.get("sync-every") {
        Some(millis) => millis
            .parse()
            .ok()
            .filter(|&millis: &u64| millis > 0)
            .map(Duration::from_millis)
            .ok_or_else(|| format!("--sync-every {millis:?} is not a number of milliseconds"))?,
        None => DEFAULT_SYNC_EVERY,
    };
    let public_key = match args.options.get("public-key") {
        Some(_) => Some(tokens::public_key(args)?),
        None => None,
    };
    let require_auth = args.options.contains_key("require-auth");
    if require_auth && public_key.is_none() {
        return Err("--require-auth needs --public-key, to check the tokens against".to_owned());
    }
    let backup = backups::place(&mut store, args)?;
    let follows = replication::primary(args)?;
    let has_replication = follows.is_some() || args.options.contains_key("feed-replicas-on");
    let committer = match (&follows, replication::wait(args)?) {
        (Some(_), _) => Committer::start_following(store),
        (None, Some(limit)) => {
            eprintln!(
                "answering a write only once a replica holds it, waiting {} s at most",
                limit.as_secs_f64()
            );
            Committer::start_waiting_for_a_replica(store, limit)
        }
        (None, None) => Committer::start(store),
    };
    let mut server = Server::new(branch, committer.writer(), committer.reader())
        .map_err(|e| e.to_string())?
        .max_connections(max_connections)
        .sync_every(sync_every)
        .require_auth(require_auth);
    if let Some(public_key) = public_key {
        server = server.public_key(public_key);
    }
    if let Some(threads) = threads {
        server = server.threads(threads);
    }
    if let Some((dir, every)) = backup {
        server = server.backup_to(dir, every);
    }
    if let Some(feed_port) = replication::feed_port(args)? {
        let feed = TcpListener::bind((address, feed_port)).map_err(|e| e.to_string())?;
        eprintln!(
            "handing the log to replicas that ask on {}",
            feed.local_addr().map_err(|e| e.to_string())?
        );
        server = server.feed_replicas_on(feed);
    }
    if let Some(primary) = follows {
        eprintln!("following {primary}: this server answers reads and takes no writes");
        if args.options.contains_key("current-values-only") {
            eprintln!(
                "keeping current values only: questions about the past are answered NOHISTORY"
            );
        }
        server = server.follow(primary);
    }
    match replication::identity(args)? {
        Some(identity) => {
            server = server.replicate_as(&identity).map_err(|e| e.to_string())?;
            eprintln!(
                "replication is over TLS, each server showing a certificate; keys are sent to \
                 replicas"
            );
        }
        None if has_replication => eprintln!("umbrachyx: {REPLICATION_IN_THE_CLEAR}"),
        None => {}
    }
    let listener = TcpListener::bind((address, port)).map_err(|e| e.to_string())?;
    if !address.is_loopback() {
        eprintln!("umbrachyx: {BEYOND_THIS_MACHINE}");
    }
    // The address it holds, not the one asked for: given port 0 the
    // system chooses, and whoever started the server reads it here.
    let serving = listener.local_addr().map_err(|e| e.to_string())?;
    eprintln!("serving {branch} on {serving}");
    server.serve(listener)
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
