//! What `serve` is told about replicas: where it hands its log to them,
//! and which server it follows itself.

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
