//! The Model Context Protocol, for one agent over standard input and
//! output: the agent's host starts `umbrachyx mcp` and exchanges JSON-RPC
//! messages with it, one per line.
//!
//! The session holds one capability token, given when it starts, and checks
//! it on every tool call before anything is read or written. Who the agent
//! is and the branch its writes go to come from the token, never from the
//! agent's requests. Only `initialize`, `ping`, `tools/list` and
//! `tools/call` are served.

mod tools;

use std::io::{self, BufRead, Write};
use std::time::SystemTime;

use biscuit_auth::PublicKey;
use serde_json::{Value, json};

use crate::capability::{self, Denied, Holder, Operation, Request};
use crate::log::Storage;
use crate::store::{Clock, Store};

/// The newest protocol version spoken, and the older ones still accepted.
const PROTOCOL_VERSION: &str = "2025-06-18";
const OLDER_VERSIONS: [&str; 2] = ["2025-03-26", "2024-11-05"];

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub struct Session<S: Storage, C: Clock> {
    store: Store<S, C>,
    token: Vec<u8>,
    root: PublicKey,
    holder: Holder,
}

impl<S: Storage, C: Clock> Session<S, C> {
    /// Refuses a token not signed by `root`, so a session never starts with
    /// one that could not allow anything.
    pub fn new(store: Store<S, C>, token: &[u8], root: PublicKey) -> Result<Self, Denied> {
        let holder = capability::holder(token, root)?;
        Ok(Self {
            store,
            token: token.to_vec(),
            root,
            holder,
        })
    }

    /// Answers requests from `input`, one per line, until it ends.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
        }
        Ok(())
    }

    /// Answers one JSON-RPC message. A notification gets no reply.
    pub fn handle(&mut self, line: &str) -> Option<String> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Some(error(&Value::Null, PARSE_ERROR, "not JSON"));
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            return Some(error(&id, INVALID_REQUEST, "not a JSON-RPC request"));
        };
        // Without an id it is a notification, such as
        // `notifications/initialized`, and nothing is sent back.
        let id = message.get("id")?;
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => Ok(initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::list() })),
            "tools/call" => self.call(&params),
            _ => Err((METHOD_NOT_FOUND, format!("no method {method:?}"))),
        };
        Some(match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
            Err((code, message)) => error(id, code, &message),
        })
    }

    fn call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let outcome = tools::call(self, name, &arguments)
            .ok_or_else(|| (INVALID_PARAMS, format!("no tool {name:?}")))?;
        // A refusal or a bad argument is the tool's result, which the agent
        // reads, not a protocol error.
        let (text, is_error) = match outcome {
            Ok(value) => (value.to_string(), false),
            Err(message) => (message, true),
        };
        Ok(json!({
            "content": [{"type": "text", "text": text}],
            "isError": is_error,
        }))
    }

    /// Asks the token whether this one key may be read or written on
    /// `branch`. The column is the last part of the key, as in
    /// `player/42/gems`. The purpose is the token's own.
    fn check(&self, operation: Operation, branch: &str, key: &str) -> Result<(), Denied> {
        let column = key.rsplit('/').next().unwrap_or(key);
        capability::authorize(
            &self.token,
            self.root,
            &Request {
                operation,
                branch,
                key,
                column,
                purpose: &self.holder.purpose,
                now: SystemTime::now(),
            },
        )
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|asked| *asked == PROTOCOL_VERSION || OLDER_VERSIONS.contains(asked))
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "umbrachyx", "version": env!("CARGO_PKG_VERSION")},
    })
}

fn error(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}
