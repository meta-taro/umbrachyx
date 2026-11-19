//! What `serve` is told about replicas: whether it hands its log to
//! them, and which server it follows itself.

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

/// Whether `--feed-replicas` was given.
pub fn feeds(args: &Args) -> bool {
    args.options.contains_key("feed-replicas")
}
