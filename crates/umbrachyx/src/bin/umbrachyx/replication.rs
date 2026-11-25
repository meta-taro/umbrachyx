//! What `serve` is told about replicas: where it hands its log to them,
//! and which server it follows itself. And `replication`, which asks a
//! running server how that is going.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::Value;
use umbrachyx::server::Identity;

use crate::Args;

/// The options that name the three files of an identity.
const IDENTITY: [&str; 3] = ["replication-cert", "replication-key", "replication-ca"];

/// What `--replication-cert`, `--replication-key` and `--replication-ca`
/// name, read, or `None` without them. All three or none: a certificate
/// without the authority the other end's must be signed by would let
/// anyone in, and one without its key shows nothing.
pub fn identity(args: &Args) -> Result<Option<Identity>, String> {
    let given: Vec<&str> = IDENTITY
        .into_iter()
        .filter(|name| args.options.contains_key(*name))
        .collect();
    if given.is_empty() {
        return Ok(None);
    }
    if let Some(missing) = IDENTITY.into_iter().find(|name| !given.contains(name)) {
        return Err(format!(
            "--{} needs --{missing} too: a certificate, its private key, and the certificate \
             of the authority that signed the other servers'",
            given[0]
        ));
    }
    if !args.options.contains_key("feed-replicas-on") && !args.options.contains_key("follow") {
        return Err(format!(
            "--{} is for a server that feeds replicas or follows another: it needs \
             --feed-replicas-on or --follow",
            given[0]
        ));
    }
    let [certificate, key, authority] = IDENTITY.map(|name| Path::new(&args.options[name]));
    warn_if_others_can_read(key);
    Identity::read(certificate, key, authority)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The server `--follow` names, or `None` without it. Only its shape is
/// checked: a replica starts whether or not its primary can be reached,
/// and answers reads with what it holds until it can.
pub fn primary(args: &Args) -> Result<Option<String>, String> {
    let Some(primary) = args.options.get("follow") else {
        return Ok(None);
    };
    let is_host_and_port = primary
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
    if !is_host_and_port {
        return Err(format!(
            "--follow {primary:?} is not a host and a port, such as 10.0.0.5:6379"
        ));
    }
    Ok(Some(primary.clone()))
}

/// How long `--wait-for-replica` says a write waits for a replica to
/// hold it, or `None` without it. It needs a port replicas ask on.
pub fn wait(args: &Args) -> Result<Option<Duration>, String> {
    let Some(seconds) = args.options.get("wait-for-replica") else {
        return Ok(None);
    };
    if !args.options.contains_key("feed-replicas-on") {
        return Err(
            "--wait-for-replica needs --feed-replicas-on, the port replicas ask on".to_owned(),
        );
    }
    if args.options.contains_key("follow") {
        return Err(
            "--wait-for-replica is for a server that takes writes, not for one that \
                    follows another"
                .to_owned(),
        );
    }
    seconds
        .parse()
        .ok()
        .filter(|&seconds: &f64| seconds > 0.0)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        .map(Some)
        .ok_or_else(|| format!("--wait-for-replica {seconds:?} is not a number of seconds"))
}

/// The port `--feed-replicas-on` names, or `None` without it.
pub fn feed_port(args: &Args) -> Result<Option<u16>, String> {
    args.options
        .get("feed-replicas-on")
        .map(|port| {
            port.parse()
                .map_err(|_| format!("--feed-replicas-on {port:?} is not a port"))
        })
        .transpose()
}

/// Says so when the private key's file can be read by other users of
/// the machine: whoever reads it can pass for this server.
#[cfg(unix)]
fn warn_if_others_can_read(key: &Path) {
    use std::os::unix::fs::PermissionsExt;

    const OTHERS: u32 = 0o077;
    let Ok(metadata) = std::fs::metadata(key) else {
        return;
    };
    if metadata.permissions().mode() & OTHERS != 0 {
        eprintln!(
            "umbrachyx: the private key in {} can be read by other users of this machine",
            key.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_others_can_read(_key: &Path) {}

/// Whether `--current-values-only` was given, which is for `serve` with
/// `--follow` only: a store that keeps no tree cannot merge, find a
/// conflict or carry out an approval, so it takes no writes of its own.
pub fn holds_current_values_only(command: &str, args: &Args) -> Result<bool, String> {
    if !args.options.contains_key("current-values-only") {
        return Ok(false);
    }
    if command != "serve" || !args.options.contains_key("follow") {
        return Err(
            "--current-values-only is for a server that follows another: it needs serve and \
             --follow"
                .to_owned(),
        );
    }
    Ok(true)
}

/// Where `replication` asks when `--host` names no other machine.
const THIS_MACHINE: &str = "127.0.0.1";
/// How long a server has to take the connection, and then to answer in
/// full.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
/// The longest status read. A real one is a few hundred bytes.
const MAX_STATUS: usize = 64 * 1024;
/// The longest text of the server's that is printed.
const MAX_SAID: usize = 300;
/// `UMBRA.REPLICATION`, as the protocol sends a command.
const ASK: &[u8] = b"*1\r\n$17\r\nUMBRA.REPLICATION\r\n";
/// The exit status when the server answered and its replication is not
/// going well: a replica that stopped or cannot reach its primary, a
/// primary that refuses writes because no replica keeps up.
pub const NOT_WELL: u8 = 1;
pub const NOT_A_STATUS: &str = "the answer is not a status: is it an Umbrachyx server?";

/// `replication`: asks the server on `--host` and `--port` how its
/// replication is going, and prints the answer a line per fact. It opens
/// no log, so it can be asked of a server that holds one. A replica a
/// few records behind is well; one that stopped is not.
pub fn status(args: &Args, default_port: u16) -> Result<ExitCode, String> {
    let [] = args.exactly()?;
    let (host, port) = server(args, default_port)?;
    let answered = ask(&host, port, ASK).map_err(|why| format!("{host}:{port}: {why}"))?;
    let (lines, is_well) =
        described(&answered).ok_or_else(|| format!("{host}:{port}: {NOT_A_STATUS}"))?;
    for line in lines {
        println!("{line}");
    }
    Ok(if is_well {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(NOT_WELL)
    })
}

/// The running server a command is to ask: the one `--host` and
/// `--port` name, on this machine and `default_port` without them.
pub fn server(args: &Args, default_port: u16) -> Result<(String, u16), String> {
    // `serve --bind A` is how the server was started; said here it would
    // be taken and this machine asked instead.
    if args.options.contains_key("bind") {
        return Err(
            "a running server is named with --host, the address it listens on, not --bind"
                .to_owned(),
        );
    }
    let host = args
        .options
        .get("host")
        .map_or(THIS_MACHINE, String::as_str);
    let port = match args.options.get("port") {
        Some(port) => port
            .parse()
            .map_err(|_| format!("--port {port:?} is not a port"))?,
        None => default_port,
    };
    Ok((host.to_owned(), port))
}

/// What the server on `host` and `port` answers to `question`, a command
/// as the protocol sends one, when the answer is a value of JSON.
/// The far end is whatever listens there: it is given a time limit, and
/// no more of its answer is read than a status could be.
pub fn ask(host: &str, port: u16, question: &[u8]) -> Result<Value, String> {
    let addresses = (host, port).to_socket_addrs().map_err(|e| e.to_string())?;
    let mut refused = "the name has no address".to_owned();
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, ANSWER_WITHIN) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => refused = error.to_string(),
        }
    }
    let mut stream = stream.ok_or(refused)?;
    let reply = answer(&mut stream, question, Instant::now() + ANSWER_WITHIN)?;
    let (first, rest) = split_line(&reply).ok_or(NOT_A_STATUS)?;
    if let Some(error) = first.strip_prefix('-') {
        return Err(format!("the server answered: {}", said(error)));
    }
    let length = value_length(first).ok_or(NOT_A_STATUS)?;
    let status = rest.get(..length).ok_or(NOT_A_STATUS)?;
    serde_json::from_slice(status).map_err(|e| format!("the answer is not JSON: {e}"))
}

/// Sends the question and reads the reply: a line, and the value after
/// it if the line says one follows. All of it by `deadline`, however
/// slowly the bytes come, and no more than a status could be.
fn answer(stream: &mut TcpStream, question: &[u8], deadline: Instant) -> Result<Vec<u8>, String> {
    let late = || format!("no answer within {} s", ANSWER_WITHIN.as_secs());
    stream
        .set_write_timeout(Some(ANSWER_WITHIN))
        .and_then(|()| stream.write_all(question))
        .map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let mut part = [0; 4096];
    while !is_whole(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(late());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        match stream.read(&mut part) {
            Ok(0) => break,
            Ok(read) => reply.extend_from_slice(&part[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(late());
            }
            Err(error) => return Err(error.to_string()),
        }
        if reply.len() > MAX_STATUS {
            return Err(NOT_A_STATUS.to_owned());
        }
    }
    Ok(reply)
}

/// The first line of `reply` without its end, and what follows it.
fn split_line(reply: &[u8]) -> Option<(&str, &[u8])> {
    let end = reply.windows(2).position(|pair| pair == b"\r\n")?;
    Some((std::str::from_utf8(&reply[..end]).ok()?, &reply[end + 2..]))
}

/// How long the value is that the line `first` says follows.
fn value_length(first: &str) -> Option<usize> {
    first.strip_prefix('$')?.parse().ok()
}

/// Whether nothing more of `reply` is to come: its line is read, and the
/// value after it if the line says how long one is.
fn is_whole(reply: &[u8]) -> bool {
    match split_line(reply) {
        None => false,
        Some((first, rest)) => value_length(first).is_none_or(|length| rest.len() >= length),
    }
}

/// Text another machine sent, fit for a terminal: no control characters,
/// and no longer than a reason needs to be.
fn said(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_SAID)
        .collect()
}

/// The lines that say `status`, and whether replication is going well;
/// `None` if it is not the status of a primary or of a replica.
fn described(status: &Value) -> Option<(Vec<String>, bool)> {
    let role = status["role"].as_str()?;
    let mut lines = vec![format!("role: {role}")];
    if let Some(holds) = status["holds"].as_str() {
        lines.push(format!("holds: {}", said(holds)));
    }
    let how = if status["encrypted"].as_bool()? {
        "over TLS"
    } else {
        "in the clear"
    };
    lines.push(match status["feeds_replicas"].as_bool()? {
        true => format!("feeds replicas: yes, {how}"),
        false => "feeds replicas: no".to_owned(),
    });
    let is_well = match role {
        "primary" => primary_lines(status, &mut lines),
        "replica" => {
            lines.push(format!("asks its primary: {how}"));
            replica_lines(status, &mut lines)
        }
        _ => return None,
    };
    Some((lines, is_well))
}

fn primary_lines(status: &Value, lines: &mut Vec<String>) -> bool {
    let records = status["records"].as_u64();
    if let Some(records) = records {
        lines.push(format!("records: {records}"));
    }
    let waits = "answers a write once a replica holds it";
    if status["waits_for_a_replica"].as_bool() != Some(true) {
        lines.push(format!("{waits}: no"));
        return true;
    }
    lines.push(match (status["replica_records"].as_u64(), records) {
        (Some(held), Some(records)) => {
            format!("{waits}: yes; a replica holds {held} of {records} records")
        }
        _ => format!("{waits}: yes"),
    });
    let refuses = status["refuses_writes"].as_bool() == Some(true);
    if refuses {
        lines.push("writes: refused until a replica holds everything".to_owned());
    }
    !refuses
}

fn replica_lines(status: &Value, lines: &mut Vec<String>) -> bool {
    let held = status["records"].as_u64();
    lines.push(match (held, status["primary_records"].as_u64()) {
        (Some(held), Some(primary)) => format!("records: {held} of the primary's {primary}"),
        _ => "records: not answered by its primary yet".to_owned(),
    });
    let unreachable = status["unreachable"].as_str();
    match unreachable {
        Some(why) => lines.push(format!("primary: unreachable: {}", said(why))),
        None if held.is_some() => lines.push("primary: answers".to_owned()),
        None => {}
    }
    let stopped = status["stopped"].as_str();
    if let Some(why) = stopped {
        lines.push(format!("stopped: {}", said(why)));
    }
    unreachable.is_none() && stopped.is_none()
}
