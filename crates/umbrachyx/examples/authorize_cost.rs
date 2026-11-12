//! What deciding one write from a capability token costs once its
//! signatures have been checked: what a connection that sent `AUTH` pays
//! for every `SET`. The key is made here and never leaves the process.
//!
//! `cargo run --release --example authorize_cost [-- <requests>]`

use std::time::{Duration, Instant, SystemTime};

use biscuit_auth::KeyPair;
use umbrachyx::capability::{self, Grant, Operation, Request, Verified};

const BRANCH: &str = "agent/bench";
const PURPOSE: &str = "bench";

fn main() {
    let requests: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(200_000);
    let root = KeyPair::new();
    let token = capability::issue(
        &root,
        &Grant {
            operations: &[Operation::Read, Operation::Write],
            key_prefix: "auth/",
            columns: &["v"],
            purpose: PURPOSE,
            expires: SystemTime::now() + Duration::from_secs(3600),
            subject: "agent:bench",
            branch: BRANCH,
        },
    )
    .expect("issues");
    let verified = Verified::new(&token, root.public()).expect("verifies");
    let keys: Vec<String> = (0..1000).map(|n| format!("auth/{n}/v")).collect();
    let now = SystemTime::now();

    let started = Instant::now();
    for n in 0..requests {
        verified
            .authorize(&Request {
                operation: Operation::Write,
                branch: BRANCH,
                key: &keys[n % keys.len()],
                column: "v",
                purpose: PURPOSE,
                now,
            })
            .expect("allowed");
    }
    let per = started.elapsed().as_nanos() / requests as u128;
    let refused = verified.authorize(&Request {
        operation: Operation::Write,
        branch: BRANCH,
        key: "other/1/v",
        column: "v",
        purpose: PURPOSE,
        now,
    });
    assert!(refused.is_err(), "a key outside the token is refused");
    println!("requests={requests} authorize_ns={per}");
}
