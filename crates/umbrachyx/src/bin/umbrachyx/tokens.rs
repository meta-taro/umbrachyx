//! Keys and capability tokens: a person makes the database's root key once,
//! issues a token to each agent, and an agent's operator can narrow a token
//! before handing it on. The agent's host then starts `umbrachyx mcp` with
//! that token.
//!
//! The private key is written to a file nobody else can read and never
//! printed. Tokens are read from standard input and written to standard
//! output, so they can go straight into a file without appearing on a
//! command line.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use biscuit_auth::{KeyPair, PrivateKey, PublicKey};
use umbrachyx::capability::{self, Grant, Narrowing, Operation};
use umbrachyx::mcp::Session;

use super::{Args, FileStore};

/// Writes a new root key to `<file>` and prints its public half.
pub fn keygen(args: &Args) -> Result<ExitCode, String> {
    let [path] = args.exactly()?;
    let key = KeyPair::new();
    let mut options = OpenOptions::new();
    // Never replaces a key: every token issued under it would stop working.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| format!("{path}: {e}"))?;
    writeln!(file, "{}", key.private().to_prefixed_string())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("{path}: {e}"))?;
    println!("{}", key.public());
    Ok(ExitCode::SUCCESS)
}

pub fn token(args: &Args) -> Result<ExitCode, String> {
    match args.positional.get(1).map(String::as_str) {
        Some("issue") => issue(args),
        Some("narrow") => narrow(args),
        Some("show") => show(args),
        _ => Err(format!(
            "token takes issue, narrow or show\n\n{}",
            super::USAGE
        )),
    }
}

/// The branch named in a token that never writes.
const NO_WRITES: &str = "-";

fn issue(args: &Args) -> Result<ExitCode, String> {
    let path = args.option("key")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let private = PrivateKey::from_str(text.trim()).map_err(|_| format!("{path}: not a key"))?;
    let operations = list(args.option("operations")?)
        .map(operation)
        .collect::<Result<Vec<_>, _>>()?;
    let columns: Vec<&str> = list(args.option("columns")?).collect();
    // A token that does not write has no branch to name; the policy only
    // reads the branch for a write.
    if operations.contains(&Operation::Write) && operations.contains(&Operation::Approve) {
        return Err(capability::TokenError::WritesAndApproves.to_string());
    }
    let branch = match args.options.get("branch") {
        Some(branch) => branch.as_str(),
        None if !operations.contains(&Operation::Write) => NO_WRITES,
        None => return Err("--branch is required for a token that writes".to_owned()),
    };
    let grant = Grant {
        operations: &operations,
        key_prefix: args.option("prefix")?,
        columns: &columns,
        purpose: args.option("purpose")?,
        expires: expiry(args.option("expires-in")?)?,
        subject: args.option("subject")?,
        branch,
    };
    let token = capability::issue(&KeyPair::from(&private), &grant).map_err(|e| e.to_string())?;
    print_token(&token)
}

fn narrow(args: &Args) -> Result<ExitCode, String> {
    let token = read_token()?;
    let narrowing = Narrowing {
        operation: args
            .options
            .get("operation")
            .map(|o| operation(o))
            .transpose()?,
        key_prefix: args.options.get("prefix").map(String::as_str),
        column: args.options.get("column").map(String::as_str),
        expires: args
            .options
            .get("expires-in")
            .map(|e| expiry(e))
            .transpose()?,
    };
    let narrowed =
        capability::narrow(&token, public_key(args)?, &narrowing).map_err(|e| e.to_string())?;
    print_token(&narrowed)
}

fn show(args: &Args) -> Result<ExitCode, String> {
    let token = read_token()?;
    let holder = capability::holder(&token, public_key(args)?).map_err(|e| e.to_string())?;
    println!("subject: {}", holder.subject);
    println!("branch: {}", holder.branch);
    println!("purpose: {}", holder.purpose);
    Ok(ExitCode::SUCCESS)
}

/// Serves one agent over standard input and output until its host closes
/// them. Messages for people go to standard error, which MCP leaves free.
pub fn mcp(store: FileStore, args: &Args) -> Result<ExitCode, String> {
    let path = args.option("token-file")?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let token = capability::from_text(&text).map_err(|e| format!("{path}: {e}"))?;
    let mut session =
        Session::new(store, &token, public_key(args)?).map_err(|e| format!("{path}: {e}"))?;
    session
        .serve(std::io::stdin().lock(), std::io::stdout().lock())
        .map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

pub(super) fn public_key(args: &Args) -> Result<PublicKey, String> {
    let text = args.option("public-key")?;
    PublicKey::from_str(text).map_err(|_| format!("--public-key {text:?} is not a public key"))
}

fn read_token() -> Result<Vec<u8>, String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| format!("standard input: {e}"))?;
    capability::from_text(&text).map_err(|e| e.to_string())
}

fn print_token(token: &[u8]) -> Result<ExitCode, String> {
    println!("{}", capability::to_text(token).map_err(|e| e.to_string())?);
    Ok(ExitCode::SUCCESS)
}

fn list(text: &str) -> impl Iterator<Item = &str> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

fn operation(name: &str) -> Result<Operation, String> {
    match name {
        "read" => Ok(Operation::Read),
        "write" => Ok(Operation::Write),
        "approve" => Ok(Operation::Approve),
        _ => Err(format!(
            "{name:?} is not an operation (read, write or approve)"
        )),
    }
}

/// Now plus a duration such as `90s`, `30m`, `8h` or `7d`.
fn expiry(text: &str) -> Result<SystemTime, String> {
    let invalid = || format!("{text:?} is not a duration such as 30m, 8h or 7d");
    let split = text
        .len()
        .checked_sub(1)
        .filter(|&at| text.is_char_boundary(at))
        .ok_or_else(invalid)?;
    let (number, unit) = text.split_at(split);
    let seconds: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    let count: u64 = number.parse().map_err(|_| invalid())?;
    count
        .checked_mul(seconds)
        .and_then(|total| SystemTime::now().checked_add(Duration::from_secs(total)))
        .ok_or_else(invalid)
}
