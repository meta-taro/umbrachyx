//! What `serve` is told about replicas: where it hands its log to them,
//! and which server it follows itself.

use std::time::Duration;

use crate::Args;

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
