//! The MCP session an agent talks to: JSON-RPC requests in, replies out,
//! every tool call checked against the agent's capability token before it
//! reads anything.
//!
//! Each test makes its own root key and store in a temporary directory.
//! The data is synthetic: players, gems and items.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use biscuit_auth::KeyPair;
use serde_json::{Value, json};
use umbrachyx::capability::{self, Denied, Grant, Narrowing, Operation};
use umbrachyx::keys::{FileKeyStorage, KeyStorage, Keys};
use umbrachyx::log::{Edit, Engine, FileStorage};
use umbrachyx::mcp::Session;
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;

const SUBJECT: &str = "agent:support";
const BRANCH: &str = "agent/support";
const PURPOSE: &str = "answer ticket 8812";
const HOUR: Duration = Duration::from_secs(3_600);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("umbrachyx-mcp-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn set(key: &str, value: &str) -> Edit {
    Edit {
        key: key.as_bytes().to_vec(),
        value: Some(value.as_bytes().to_vec()),
    }
}

fn by(who: &str, why: &str) -> Provenance {
    Provenance {
        who: who.to_owned(),
        why: why.to_owned(),
        approved_by: None,
    }
}

/// Main with two players, written by people before any agent connects.
fn store(dir: &TempDir) -> FileStore {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    store
        .commit(
            MAIN_BRANCH,
            vec![
                set("player/42/gems", "120"),
                set("player/42/item", "sword"),
                set("player/42/email", "p42@example.test"),
                set("player/7/gems", "30"),
            ],
            by("game:onboarding", "starter grant"),
        )
        .expect("commits");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("game:shop", "bought a potion"),
        )
        .expect("commits");
    store
}

/// Player 42's gems and items, read and write, for an hour.
fn grant(expires: SystemTime) -> Grant<'static> {
    Grant {
        operations: &[Operation::Read, Operation::Write],
        key_prefix: "player/42/",
        columns: &["gems", "item"],
        purpose: PURPOSE,
        expires,
        subject: SUBJECT,
        branch: BRANCH,
    }
}

fn session(store: FileStore, expires: SystemTime) -> Session<FileStorage, SystemClock> {
    let root = KeyPair::new();
    let token = capability::issue(&root, &grant(expires)).expect("issues");
    Session::new(store, &token, root.public()).expect("starts")
}

/// Sends one request and returns the reply.
fn ask(session: &mut Session<FileStorage, SystemClock>, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let reply = session
        .handle(&request.to_string())
        .expect("a request gets a reply");
    serde_json::from_str(&reply).expect("replies with JSON")
}

/// Calls a tool and returns its text and whether it is an error.
fn call(
    session: &mut Session<FileStorage, SystemClock>,
    tool: &str,
    arguments: Value,
) -> (Value, bool) {
    let reply = ask(
        session,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    );
    let result = &reply["result"];
    let text = result["content"][0]["text"].as_str().expect("has text");
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let body = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
    (body, is_error)
}

#[test]
fn initialize_and_list_the_tools() {
    let dir = TempDir::new("initialize");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);

    let init = ask(
        &mut session,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    let initialized = session.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    let tools = ask(&mut session, "tools/list", json!({}));

    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "umbrachyx");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert_eq!(initialized, None, "a notification gets no reply");
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("lists tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("named"))
        .collect();
    for name in ["get", "history", "diff", "commit"] {
        assert!(names.contains(&name), "{name} in {names:?}");
    }
    for tool in tools["result"]["tools"].as_array().expect("lists tools") {
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
}

#[test]
fn get_reads_inside_the_token_and_is_refused_outside() {
    let dir = TempDir::new("get");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);

    let gems = call(&mut session, "get", json!({"key": "player/42/gems"}));
    let missing = call(&mut session, "get", json!({"key": "player/42/item/shield"}));
    let other_player = call(&mut session, "get", json!({"key": "player/7/gems"}));
    let other_column = call(&mut session, "get", json!({"key": "player/42/email"}));

    assert_eq!(
        gems,
        (
            json!({"branch": "main", "key": "player/42/gems", "value": "95", "erased": false}),
            false
        )
    );
    assert_eq!(missing.0["value"], Value::Null);
    let refusal = (
        Value::String(format!("refused: {}", Denied::OutOfScope)),
        true,
    );
    assert_eq!(other_player, refusal);
    assert_eq!(other_column, refusal);
}

#[test]
fn history_shows_who_changed_a_value_and_why() {
    let dir = TempDir::new("history");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);

    let (history, is_error) = call(&mut session, "history", json!({"key": "player/42/gems"}));
    let refused = call(&mut session, "history", json!({"key": "player/7/gems"}));

    assert!(!is_error, "{history}");
    let changes = history["changes"].as_array().expect("lists changes");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["value"], "95");
    assert_eq!(changes[0]["who"], "game:shop");
    assert_eq!(changes[0]["why"], "bought a potion");
    assert_eq!(changes[1]["value"], "120");
    assert_eq!(changes[1]["who"], "game:onboarding");
    assert!(refused.1);
}

#[test]
fn diff_shows_only_the_keys_the_token_can_read() {
    let dir = TempDir::new("diff");
    let mut store = store(&dir);
    store.create_branch("event", MAIN_BRANCH).expect("branches");
    store
        .commit(
            "event",
            vec![
                set("player/42/gems", "5095"),
                set("player/42/email", "changed@example.test"),
                set("player/7/gems", "1030"),
            ],
            by("ops:events", "halloween reward"),
        )
        .expect("commits");
    let mut session = session(store, SystemTime::now() + HOUR);

    let (diff, is_error) = call(&mut session, "diff", json!({"from": "main", "to": "event"}));

    assert!(!is_error, "{diff}");
    assert_eq!(
        diff["changes"],
        json!([{"key": "player/42/gems", "before": "95", "after": "5095", "erased": false}])
    );
    assert_eq!(diff["omitted_tables"], json!([]));
}

#[test]
fn reads_go_to_the_tokens_branch_once_it_exists() {
    let dir = TempDir::new("branch");
    let mut store = store(&dir);
    store.create_branch(BRANCH, MAIN_BRANCH).expect("branches");
    store
        .commit(
            BRANCH,
            vec![set("player/42/gems", "100")],
            by(SUBJECT, "refund"),
        )
        .expect("commits");
    let mut session = session(store, SystemTime::now() + HOUR);

    let own = call(&mut session, "get", json!({"key": "player/42/gems"}));
    let main = call(
        &mut session,
        "get",
        json!({"key": "player/42/gems", "branch": "main"}),
    );

    assert_eq!(own.0["branch"], BRANCH);
    assert_eq!(own.0["value"], "100");
    assert_eq!(main.0["value"], "95");
}

#[test]
fn an_expired_token_reads_nothing() {
    let dir = TempDir::new("expired");
    let mut session = session(store(&dir), SystemTime::now() - Duration::from_secs(1));

    let refused = call(&mut session, "get", json!({"key": "player/42/gems"}));

    assert_eq!(
        refused,
        (Value::String(format!("refused: {}", Denied::Expired)), true)
    );
}

#[test]
fn a_token_from_another_root_does_not_start_a_session() {
    let dir = TempDir::new("foreign");
    let token =
        capability::issue(&KeyPair::new(), &grant(SystemTime::now() + HOUR)).expect("issues");

    let started = Session::new(store(&dir), &token, KeyPair::new().public());

    assert_eq!(started.err(), Some(Denied::Invalid));
}

#[test]
fn malformed_and_unknown_requests_get_json_rpc_errors() {
    let dir = TempDir::new("errors");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);

    let garbage: Value =
        serde_json::from_str(&session.handle("{not json").expect("replies")).expect("JSON");
    let unknown = ask(&mut session, "resources/list", json!({}));
    let no_tool = ask(
        &mut session,
        "tools/call",
        json!({"name": "drop_table", "arguments": {}}),
    );
    let no_key = call(&mut session, "get", json!({}));

    assert_eq!(garbage["error"]["code"], -32700);
    assert_eq!(garbage["id"], Value::Null);
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(no_tool["error"]["code"], -32602);
    assert!(no_key.1);
}

#[test]
fn a_commit_lands_on_the_tokens_branch_with_the_agent_as_author() {
    let dir = TempDir::new("commit");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);

    let (written, is_error) = call(
        &mut session,
        "commit",
        json!({"set": {"player/42/gems": "100"}, "remove": ["player/42/item"], "why": "refund for ticket 8812"}),
    );
    let main = call(
        &mut session,
        "get",
        json!({"key": "player/42/gems", "branch": "main"}),
    );
    let own = call(&mut session, "get", json!({"key": "player/42/gems"}));
    let item = call(&mut session, "get", json!({"key": "player/42/item"}));
    let (history, _) = call(&mut session, "history", json!({"key": "player/42/gems"}));

    assert!(!is_error, "{written}");
    assert_eq!(written["branch"], BRANCH);
    assert_eq!(main.0["value"], "95", "main is untouched");
    assert_eq!(own.0["value"], "100");
    assert_eq!(item.0["value"], Value::Null);
    assert_eq!(history["changes"][0]["who"], SUBJECT);
    assert_eq!(history["changes"][0]["why"], "refund for ticket 8812");
    assert_eq!(history["changes"][0]["approved_by"], serde_json::json!([]));
    assert_eq!(history["changes"][0]["commit"], written["commit"]);
}

#[test]
fn a_commit_with_one_key_outside_the_token_writes_nothing() {
    let dir = TempDir::new("commit-scope");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);

    let refused = call(
        &mut session,
        "commit",
        json!({"set": {"player/42/gems": "100", "player/7/gems": "999999"}, "why": "refund"}),
    );
    let own = call(&mut session, "get", json!({"key": "player/42/gems"}));

    assert_eq!(
        refused,
        (
            Value::String(format!("refused: {}", Denied::OutOfScope)),
            true
        )
    );
    assert_eq!(own.0["branch"], "main", "no branch was made");
    assert_eq!(own.0["value"], "95");
}

#[test]
fn a_commit_names_no_branch_and_needs_a_reason() {
    let dir = TempDir::new("commit-arguments");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);
    let set = json!({"player/42/gems": "100"});

    let to_main = call(
        &mut session,
        "commit",
        json!({"set": set, "why": "refund", "branch": "main"}),
    );
    let no_reason = call(&mut session, "commit", json!({"set": set}));
    let blank_reason = call(&mut session, "commit", json!({"set": set, "why": "  "}));
    let nothing = call(&mut session, "commit", json!({"why": "refund"}));
    let not_text = call(
        &mut session,
        "commit",
        json!({"set": {"player/42/gems": 100}, "why": "refund"}),
    );
    let main = call(
        &mut session,
        "get",
        json!({"key": "player/42/gems", "branch": "main"}),
    );

    for refused in [&to_main, &no_reason, &blank_reason, &nothing, &not_text] {
        assert!(refused.1, "{refused:?}");
    }
    assert_eq!(main.0["value"], "95");
}

#[test]
fn a_read_only_token_cannot_commit() {
    let dir = TempDir::new("commit-read-only");
    let root = KeyPair::new();
    let token = capability::issue(&root, &grant(SystemTime::now() + HOUR)).expect("issues");
    let read_only = capability::narrow(
        &token,
        root.public(),
        &Narrowing {
            operation: Some(Operation::Read),
            ..Narrowing::default()
        },
    )
    .expect("narrows");
    let mut session = Session::new(store(&dir), &read_only, root.public()).expect("starts");

    let refused = call(
        &mut session,
        "commit",
        json!({"set": {"player/42/gems": "100"}, "why": "refund"}),
    );
    let read = call(&mut session, "get", json!({"key": "player/42/gems"}));

    assert_eq!(
        refused,
        (
            Value::String(format!("refused: {}", Denied::OutOfScope)),
            true
        )
    );
    assert_eq!(read.0["value"], "95");
}

#[test]
fn a_line_past_the_size_limit_is_refused_and_the_session_goes_on() {
    let dir = TempDir::new("size");
    let mut session = session(store(&dir), SystemTime::now() + HOUR);
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"get","arguments":{{"key":"{}"}}}}}}"#,
        "x".repeat(umbrachyx::mcp::MAX_LINE)
    );
    let next = r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#;
    let input = format!("{huge}\n{next}\n");
    let mut output = Vec::new();

    session
        .serve(input.as_bytes(), &mut output)
        .expect("serves");

    let replies: Vec<Value> = String::from_utf8(output)
        .expect("text")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert_eq!(replies[0]["error"]["code"], -32600);
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["result"], json!({}));
}

/// An erased player's values come back as erased, not as absent or removed.
#[test]
fn erased_values_read_as_erased() {
    let dir = TempDir::new("erased");
    let key_storage: Box<dyn KeyStorage + Send> =
        Box::new(FileKeyStorage::new(&dir.0.join("log.keys")));
    let keys = Keys::open(key_storage).expect("opens the key file");
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open_sealed(storage, keys, SystemClock).expect("opens");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "120")],
            by("game:onboarding", "starter grant"),
        )
        .expect("commits");
    let first = store.head(MAIN_BRANCH).expect("has a head");
    store
        .commit(
            MAIN_BRANCH,
            vec![set("player/42/gems", "95")],
            by("game:shop", "bought a potion"),
        )
        .expect("commits");
    store
        .erase(b"player/42", by("ops:privacy", "account deletion request"))
        .expect("erases");
    let mut session = session(store, SystemTime::now() + HOUR);
    let from = umbrachyx::store::short(&first);

    let get = call(&mut session, "get", json!({"key": "player/42/gems"}));
    let (history, _) = call(&mut session, "history", json!({"key": "player/42/gems"}));
    let (diff, _) = call(&mut session, "diff", json!({"from": from, "to": "main"}));

    assert_eq!(get.0["value"], Value::Null);
    assert_eq!(get.0["erased"], true);
    assert_eq!(history["changes"][0]["erased"], true);
    assert_eq!(history["changes"][0]["why"], "bought a potion");
    assert_eq!(history["changes"][1]["erased"], true);
    assert_eq!(diff["changes"][0]["erased"], true);
}

/// Main where `player` and `pos` keep only latest values, written twice.
fn latest_store(dir: &TempDir) -> FileStore {
    let storage = FileStorage::open(&dir.0.join("log")).expect("opens file");
    let mut store = Store::open(storage, SystemClock).expect("opens store");
    for table in ["player", "pos"] {
        store
            .declare_table(table.as_bytes(), Engine::Latest, by("ops", "hot table"))
            .expect("declares");
    }
    for gems in ["120", "95"] {
        store
            .commit(
                MAIN_BRANCH,
                vec![set("player/42/gems", gems), set("pos/42", gems)],
                by("game:shop", "tick"),
            )
            .expect("commits");
    }
    store
}

#[test]
fn history_of_a_latest_key_is_refused_as_having_none() {
    let dir = TempDir::new("latest-history");
    let mut session = session(latest_store(&dir), SystemTime::now() + HOUR);

    let (history, is_error) = call(&mut session, "history", json!({"key": "player/42/gems"}));

    assert!(is_error);
    assert_eq!(history, "table 'player' keeps only the latest value");
    assert_eq!(
        call(&mut session, "get", json!({"key": "player/42/gems"})).0["value"],
        "95"
    );
}

/// The agent learns which of its tables a diff left out, and nothing of
/// tables outside its token.
#[test]
fn diff_names_the_left_out_tables_the_token_reaches() {
    let dir = TempDir::new("latest-diff");
    let mut session = session(latest_store(&dir), SystemTime::now() + HOUR);

    let (diff, is_error) = call(
        &mut session,
        "diff",
        json!({"from": "main~1", "to": "main"}),
    );

    assert!(!is_error, "{diff}");
    assert_eq!(diff["changes"], json!([]));
    assert_eq!(diff["omitted_tables"], json!(["player"]));
}

/// A session whose token was narrowed by its holder before it connected.
fn narrowed_session(
    store: FileStore,
    narrowing: &Narrowing<'_>,
) -> Session<FileStorage, SystemClock> {
    let root = KeyPair::new();
    let token = capability::issue(&root, &grant(SystemTime::now() + HOUR)).expect("issues");
    let narrowed = capability::narrow(&token, root.public(), narrowing).expect("narrows");
    Session::new(store, &narrowed, root.public()).expect("starts")
}

/// Even a diff with no change the token can see checks the token, so an
/// expired one learns no table names.
#[test]
fn an_expired_token_is_refused_a_diff_that_only_left_tables_out() {
    let dir = TempDir::new("latest-diff-expired");
    let mut session = session(latest_store(&dir), SystemTime::now() - HOUR);

    let (diff, is_error) = call(
        &mut session,
        "diff",
        json!({"from": "main~1", "to": "main"}),
    );

    assert!(is_error, "{diff}");
    assert_eq!(diff, format!("refused: {}", Denied::Expired));
}

/// A table is named only when the token, narrowings and all, may read a
/// key of it.
#[test]
fn a_narrowed_token_is_told_of_no_table_it_cannot_read() {
    let dir = TempDir::new("latest-diff-narrowed");
    for narrowing in [
        Narrowing {
            operation: Some(Operation::Write),
            ..Narrowing::default()
        },
        Narrowing {
            key_prefix: Some("player/7/"),
            ..Narrowing::default()
        },
    ] {
        let mut session = narrowed_session(latest_store(&dir), &narrowing);
        let (diff, is_error) = call(
            &mut session,
            "diff",
            json!({"from": "main~1", "to": "main"}),
        );
        assert!(!is_error, "{diff}");
        assert_eq!(diff["omitted_tables"], json!([]), "{narrowing:?}");
        std::fs::remove_file(dir.0.join("log")).expect("starts the next store afresh");
    }
}

#[test]
fn a_narrowed_token_is_told_of_a_table_it_can_still_read() {
    let dir = TempDir::new("latest-diff-narrowed-reads");
    let narrowing = Narrowing {
        key_prefix: Some("player/42/item"),
        column: Some("item"),
        ..Narrowing::default()
    };
    let mut session = narrowed_session(latest_store(&dir), &narrowing);

    let (diff, is_error) = call(
        &mut session,
        "diff",
        json!({"from": "main~1", "to": "main"}),
    );

    assert!(!is_error, "{diff}");
    assert_eq!(diff["omitted_tables"], json!(["player"]));
}
