//! What `serve` is told about replicas: where it hands its log to them,
//! and which server it follows itself.

use std::path::Path;
use std::time::Duration;

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
