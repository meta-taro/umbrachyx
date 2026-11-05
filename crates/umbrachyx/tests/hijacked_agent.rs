//! What a hijacked agent can do through the MCP session: everything a
//! prompt injection could make it try, with a valid token for player 42's
//! gems and items. Each attack is refused, and after each one the log is
//! opened again to show that main and every other branch are exactly as
//! they were.
//!
//! The results are published in `docs/agents.md`. The data is synthetic.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use biscuit_auth::{Biscuit, BlockBuilder, KeyPair, PublicKey};
use serde_json::{Value, json};
use umbrachyx::capability::{self, Denied, Grant, Operation};
use umbrachyx::log::{Edit, FileStorage};
use umbrachyx::mcp::Session;
use umbrachyx::store::{MAIN_BRANCH, Provenance, Store, SystemClock};

type FileStore = Store<FileStorage, SystemClock>;
type AgentSession = Session<FileStorage, SystemClock>;
/// Every branch and its live keys and values.
type State = BTreeMap<String, BTreeMap<String, String>>;

const SUBJECT: &str = "agent:support";
const BRANCH: &str = "agent/support";
const OTHER_AGENTS_BRANCH: &str = "agent/events";
const HOUR: Duration = Duration::from_secs(3_600);

/// A fresh directory per test, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("umbrachyx-hijack-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates temp dir");
        Self(dir)
    }

    fn log(&self) -> PathBuf {
        self.0.join("log")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(log: &Path) -> FileStore {
    Store::open(FileStorage::open(log).expect("opens file"), SystemClock).expect("opens store")
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

/// Live data on main and another agent's branch, written before the
/// hijacked agent connects.
fn seeded(dir: &TempDir) -> FileStore {
    let mut store = open(&dir.log());
    store
        .commit(
            MAIN_BRANCH,
            vec![
                set("player/42/gems", "95"),
                set("player/42/item", "sword"),
                set("player/42/email", "p42@example.test"),
                set("player/7/gems", "30"),
                set("player/7/email", "p7@example.test"),
            ],
            by("game:onboarding", "starter grant"),
        )
        .expect("commits");
    store
        .create_branch(OTHER_AGENTS_BRANCH, MAIN_BRANCH)
        .expect("branches");
    store
        .commit(
            OTHER_AGENTS_BRANCH,
            vec![set("player/7/gems", "1030")],
            by("agent:events", "halloween reward"),
        )
        .expect("commits");
    store
}

/// Every branch as the log has it, read by opening the log again.
fn state(dir: &TempDir) -> State {
    let store = open(&dir.log());
    store
        .branches()
        .map(|branch| {
            let entries = store
                .entries(branch)
                .into_iter()
                .map(|(key, value)| {
                    (
                        String::from_utf8_lossy(&key).into_owned(),
                        String::from_utf8_lossy(&value).into_owned(),
                    )
                })
                .collect();
            (branch.to_owned(), entries)
        })
        .collect()
}

fn grant(expires: SystemTime) -> Grant<'static> {
    Grant {
        operations: &[Operation::Read, Operation::Write],
        key_prefix: "player/42/",
        columns: &["gems", "item"],
        purpose: "answer ticket 8812",
        expires,
        subject: SUBJECT,
        branch: BRANCH,
    }
}

struct Agent {
    session: AgentSession,
}

impl Agent {
    fn start(store: FileStore, token: &[u8], root: PublicKey) -> Self {
        Self {
            session: Session::new(store, token, root).expect("starts"),
        }
    }

    /// Sends a raw line, as a hijacked agent can, and returns the reply.
    fn send(&mut self, line: &str) -> Option<Value> {
        self.session
            .handle(line)
            .map(|reply| serde_json::from_str(&reply).expect("replies with JSON"))
    }

    /// Calls a tool and returns its text and whether it is an error.
    fn call(&mut self, tool: &str, arguments: Value) -> (String, bool) {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": {"name": tool, "arguments": arguments}});
        let reply = self.send(&request.to_string()).expect("replies");
        let result = &reply["result"];
        (
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            result["isError"].as_bool().unwrap_or(false),
        )
    }

    fn refused(&mut self, tool: &str, arguments: Value) -> String {
        let (text, is_error) = self.call(tool, arguments);
        assert!(is_error, "{tool} was not refused: {text}");
        text
    }
}

/// Starts an agent with an ordinary token, runs `attack`, then checks that
/// the log holds exactly what it held before.
fn attack(name: &str, attack: impl FnOnce(&mut Agent)) {
    let dir = TempDir::new(name);
    let store = seeded(&dir);
    let root = KeyPair::new();
    let token = capability::issue(&root, &grant(SystemTime::now() + HOUR)).expect("issues");
    let mut agent = Agent::start(store, &token, root.public());
    attack(&mut agent);
    // Closing the session releases the log, so it can be read afresh.
    drop(agent);
    assert_eq!(state(&dir), expected_untouched());
}

fn expected_untouched() -> State {
    let main: BTreeMap<String, String> = [
        ("player/42/email", "p42@example.test"),
        ("player/42/gems", "95"),
        ("player/42/item", "sword"),
        ("player/7/email", "p7@example.test"),
        ("player/7/gems", "30"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let mut other = main.clone();
    other.insert("player/7/gems".to_owned(), "1030".to_owned());
    [
        (MAIN_BRANCH.to_owned(), main),
        (OTHER_AGENTS_BRANCH.to_owned(), other),
    ]
    .into_iter()
    .collect()
}

fn out_of_scope() -> String {
    format!("refused: {}", Denied::OutOfScope)
}

#[test]
fn reading_another_players_row_is_refused() {
    attack("other-row", |agent| {
        assert_eq!(
            agent.refused("get", json!({"key": "player/7/gems"})),
            out_of_scope()
        );
        assert_eq!(
            agent.refused("history", json!({"key": "player/7/gems"})),
            out_of_scope()
        );
        assert_eq!(
            agent.refused(
                "get",
                json!({"key": "player/7/gems", "branch": OTHER_AGENTS_BRANCH})
            ),
            out_of_scope()
        );
    });
}

#[test]
fn reading_a_column_outside_the_token_is_refused() {
    attack("other-column", |agent| {
        assert_eq!(
            agent.refused("get", json!({"key": "player/42/email"})),
            out_of_scope()
        );
        assert_eq!(
            agent.refused("history", json!({"key": "player/42/email"})),
            out_of_scope()
        );
    });
}

#[test]
fn a_key_that_only_looks_like_the_players_is_refused() {
    attack("lookalike", |agent| {
        for key in [
            "player/420/gems",
            "player/42",
            "player/4",
            "player/42/../7/gems",
        ] {
            let (text, is_error) = agent.call("get", json!({"key": key}));
            // `player/42/../7/gems` is inside the prefix as text, and keys
            // are names, not paths: it names a key no one wrote.
            if key == "player/42/../7/gems" {
                assert!(!is_error, "{text}");
                assert!(text.contains(r#""value":null"#), "{text}");
            } else {
                assert!(is_error, "{key}: {text}");
            }
        }
    });
}

#[test]
fn a_diff_does_not_reveal_other_players_changes() {
    attack("diff", |agent| {
        let (text, is_error) =
            agent.call("diff", json!({"from": "main", "to": OTHER_AGENTS_BRANCH}));

        assert!(!is_error, "{text}");
        assert!(!text.contains("player/7"), "{text}");
        assert!(!text.contains("1030"), "{text}");
    });
}

#[test]
fn writing_to_main_is_refused() {
    attack("main", |agent| {
        agent.refused(
            "commit",
            json!({"set": {"player/42/gems": "999999"}, "why": "x", "branch": "main"}),
        );
        agent.refused(
            "commit",
            json!({"set": {"player/42/gems": "999999"}, "why": "x", "target": "main"}),
        );
    });
}

#[test]
fn writing_to_another_agents_branch_is_refused() {
    attack("other-branch", |agent| {
        agent.refused(
            "commit",
            json!({"set": {"player/42/gems": "999999"}, "why": "x", "branch": OTHER_AGENTS_BRANCH}),
        );
    });
}

#[test]
fn writing_outside_the_token_writes_nothing_at_all() {
    attack("write-scope", |agent| {
        for set in [
            json!({"player/7/gems": "999999"}),
            json!({"player/42/email": "attacker@example.test"}),
            json!({"player/42/gems": "100", "player/7/gems": "999999"}),
        ] {
            assert_eq!(
                agent.refused("commit", json!({"set": set, "why": "x"})),
                out_of_scope()
            );
        }
        assert_eq!(
            agent.refused("commit", json!({"remove": ["player/7/gems"], "why": "x"})),
            out_of_scope()
        );
    });
}

#[test]
fn merging_or_any_tool_not_offered_is_refused() {
    attack("no-tool", |agent| {
        for tool in ["merge", "revert", "branch", "token", "set_approved_by"] {
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                 "params": {"name": tool, "arguments": {"into": "main", "from": BRANCH}}});
            let reply = agent.send(&request.to_string()).expect("replies");
            assert_eq!(reply["error"]["code"], -32602, "{tool}: {reply}");
        }
    });
}

#[test]
fn malformed_messages_change_nothing_and_the_session_goes_on() {
    attack("garbage", |agent| {
        for line in [
            "{",
            "[]",
            r#"[{"jsonrpc":"2.0","id":1,"method":"tools/call"}]"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":"commit"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"commit","arguments":[]}}"#,
            "\u{0}\u{1}\u{2}",
        ] {
            let reply = agent.send(line).expect("replies");
            assert!(
                reply.get("error").is_some() || reply["result"]["isError"] == true,
                "{line}: {reply}"
            );
        }
        let (text, is_error) = agent.call("get", json!({"key": "player/42/gems"}));
        assert!(!is_error, "{text}");
    });
}

/// The holder appends a block claiming to be someone else, to write to
/// main, to every player and every column: the issuer's block still decides.
#[test]
fn a_token_widened_by_its_holder_allows_nothing_more() {
    let dir = TempDir::new("widened");
    let store = seeded(&dir);
    let root = KeyPair::new();
    let token = capability::issue(&root, &grant(SystemTime::now() + HOUR)).expect("issues");
    let forged = BlockBuilder::new()
        .code_with_params(
            r#"
            subject("ops:lead");
            write_branch("main");
            key_prefix("player/");
            column_allowed("email");
            operation_allowed("write");
            "#,
            HashMap::new(),
            HashMap::new(),
        )
        .expect("parses");
    let widened = Biscuit::from(&token, root.public())
        .and_then(|token| token.append(forged))
        .and_then(|token| token.to_vec())
        .expect("appends");
    let mut agent = Agent::start(store, &widened, root.public());

    assert_eq!(
        agent.refused("get", json!({"key": "player/7/gems"})),
        out_of_scope()
    );
    assert_eq!(
        agent.refused("get", json!({"key": "player/42/email"})),
        out_of_scope()
    );
    let (written, is_error) = agent.call(
        "commit",
        json!({"set": {"player/42/gems": "100"}, "why": "refund"}),
    );
    drop(agent);

    // The one write inside the token went to the agent's branch, under the
    // agent's name, not to main under the name it claimed.
    assert!(!is_error, "{written}");
    let author = open(&dir.log())
        .log(BRANCH)
        .first()
        .map(|info| info.provenance.who.clone());
    assert_eq!(author.as_deref(), Some(SUBJECT));
    let mut expected = expected_untouched();
    let mut own = expected[MAIN_BRANCH].clone();
    own.insert("player/42/gems".to_owned(), "100".to_owned());
    expected.insert(BRANCH.to_owned(), own);
    assert_eq!(state(&dir), expected);
}

#[test]
fn a_forged_or_altered_token_does_not_start_a_session() {
    let dir = TempDir::new("forged");
    let root = KeyPair::new();
    let forged =
        capability::issue(&KeyPair::new(), &grant(SystemTime::now() + HOUR)).expect("issues");
    let mut altered = capability::issue(&root, &grant(SystemTime::now() + HOUR)).expect("issues");
    let middle = altered.len() / 2;
    altered[middle] ^= 1;

    for token in [forged.as_slice(), altered.as_slice(), b"admin".as_slice()] {
        let started = Session::new(seeded_once(&dir), token, root.public());
        assert_eq!(started.err(), Some(Denied::Invalid));
    }
    assert_eq!(state(&dir), expected_untouched());
}

/// Seeds the log the first time and only opens it afterwards.
fn seeded_once(dir: &TempDir) -> FileStore {
    let store = open(&dir.log());
    if store.head(MAIN_BRANCH).is_some() {
        return store;
    }
    drop(store);
    seeded(dir)
}

#[test]
fn an_expired_token_reads_and_writes_nothing() {
    let dir = TempDir::new("expired");
    let store = seeded(&dir);
    let root = KeyPair::new();
    let token = capability::issue(&root, &grant(SystemTime::now() - Duration::from_secs(1)))
        .expect("issues");
    let mut agent = Agent::start(store, &token, root.public());
    let expired = format!("refused: {}", Denied::Expired);

    assert_eq!(
        agent.refused("get", json!({"key": "player/42/gems"})),
        expired
    );
    assert_eq!(
        agent.refused("diff", json!({"from": "main", "to": OTHER_AGENTS_BRANCH})),
        expired
    );
    assert_eq!(
        agent.refused(
            "commit",
            json!({"set": {"player/42/gems": "100"}, "why": "x"})
        ),
        expired
    );
    drop(agent);
    assert_eq!(state(&dir), expected_untouched());
}
