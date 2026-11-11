//! Capability tokens: requests outside a token are refused, a token can be
//! narrowed but not widened, an expired token is refused, and writes land
//! only on the branch the token names.
//!
//! Each test makes its own root key pair in memory and drops it at the end.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use biscuit_auth::{Biscuit, BlockBuilder, KeyPair};
use umbrachyx::capability::{
    self, Denied, Grant, Holder, MAIN_BRANCH, Narrowing, Operation, Request,
};

const PLAYER: &str = "player/00000042/";
const OTHER_PLAYER_SLOT: &str = "player/00000043/slot/001";
const PURPOSE: &str = "restock event";
const BRANCH: &str = "agent/restock";
const SUBJECT: &str = "agent:restock";
const HOUR: Duration = Duration::from_secs(3_600);

/// A fixed instant, so expiry does not depend on when the test runs.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn grant() -> Grant<'static> {
    Grant {
        operations: &[Operation::Read, Operation::Write],
        key_prefix: PLAYER,
        columns: &["item", "count"],
        purpose: PURPOSE,
        expires: now() + HOUR,
        subject: SUBJECT,
        branch: BRANCH,
    }
}

fn request<'a>(operation: Operation, key: &'a str, column: &'a str) -> Request<'a> {
    Request {
        operation,
        branch: BRANCH,
        key,
        column,
        purpose: PURPOSE,
        now: now(),
    }
}

fn slot(n: u32) -> String {
    format!("{PLAYER}slot/{n:03}")
}

fn issue(root: &KeyPair, grant: &Grant<'_>) -> Vec<u8> {
    capability::issue(root, grant).expect("issues")
}

#[test]
fn requests_inside_the_token_are_allowed() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let check = |request: &Request<'_>| capability::authorize(&token, root.public(), request);

    assert_eq!(check(&request(Operation::Read, &slot(1), "count")), Ok(()));
    assert_eq!(check(&request(Operation::Write, &slot(1), "item")), Ok(()));
}

#[test]
fn rows_and_columns_outside_the_token_are_refused() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let check = |request: &Request<'_>| capability::authorize(&token, root.public(), request);

    for operation in [Operation::Read, Operation::Write] {
        let other_row = request(operation, OTHER_PLAYER_SLOT, "count");
        let key = slot(1);
        let other_column = request(operation, &key, "paid_currency");
        // A key that shares the prefix's characters but not the player.
        let lookalike = request(operation, "player/000000420/slot/001", "count");

        assert_eq!(check(&other_row), Err(Denied::OutOfScope));
        assert_eq!(check(&other_column), Err(Denied::OutOfScope));
        assert_eq!(check(&lookalike), Err(Denied::OutOfScope));
    }
}

#[test]
fn an_operation_or_purpose_outside_the_token_is_refused() {
    let root = KeyPair::new();
    let read_only = issue(
        &root,
        &Grant {
            operations: &[Operation::Read],
            ..grant()
        },
    );
    let token = issue(&root, &grant());
    let key = slot(1);

    let write = capability::authorize(
        &read_only,
        root.public(),
        &request(Operation::Write, &key, "count"),
    );
    let other_purpose = capability::authorize(
        &token,
        root.public(),
        &Request {
            purpose: "analytics",
            ..request(Operation::Read, &key, "count")
        },
    );

    assert_eq!(write, Err(Denied::OutOfScope));
    assert_eq!(other_purpose, Err(Denied::OutOfScope));
}

#[test]
fn writes_to_main_are_refused_and_reads_are_not() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let key = slot(1);
    let on_main = |operation| Request {
        branch: MAIN_BRANCH,
        ..request(operation, &key, "count")
    };

    let write = capability::authorize(&token, root.public(), &on_main(Operation::Write));
    let read = capability::authorize(&token, root.public(), &on_main(Operation::Read));

    assert_eq!(write, Err(Denied::MainBranch));
    assert_eq!(read, Ok(()));
}

#[test]
fn an_expired_token_is_refused() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let key = slot(1);
    let at = |now, key| {
        capability::authorize(
            &token,
            root.public(),
            &Request {
                now,
                ..request(Operation::Read, key, "count")
            },
        )
    };

    assert_eq!(at(now() + HOUR, key.as_str()), Ok(()));
    assert_eq!(
        at(now() + HOUR + Duration::from_secs(1), key.as_str()),
        Err(Denied::Expired)
    );
    // Expiry is reported first, even when the request is also out of scope.
    assert_eq!(
        at(now() + 2 * HOUR, OTHER_PLAYER_SLOT),
        Err(Denied::Expired)
    );
}

#[test]
fn a_narrowed_token_allows_only_the_narrower_scope() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let prefix = format!("{PLAYER}slot/00");
    let narrowed = capability::narrow(
        &token,
        root.public(),
        &Narrowing {
            operation: Some(Operation::Read),
            key_prefix: Some(&prefix),
            column: Some("count"),
            expires: Some(now() + Duration::from_secs(60)),
        },
    )
    .expect("narrows");
    let check = |request: &Request<'_>| capability::authorize(&narrowed, root.public(), request);
    let (inside, outside) = (slot(5), slot(50));

    assert_eq!(check(&request(Operation::Read, &inside, "count")), Ok(()));
    assert_eq!(
        check(&request(Operation::Read, &outside, "count")),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&request(Operation::Read, &inside, "item")),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&request(Operation::Write, &inside, "count")),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&Request {
            now: now() + Duration::from_secs(61),
            ..request(Operation::Read, &inside, "count")
        }),
        Err(Denied::Expired)
    );
}

#[test]
fn narrowing_to_a_wider_scope_does_not_widen() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let wider = capability::narrow(
        &token,
        root.public(),
        &Narrowing {
            key_prefix: Some("player/"),
            column: Some("paid_currency"),
            expires: Some(now() + 100 * HOUR),
            ..Narrowing::default()
        },
    )
    .expect("appends");
    let check = |request: &Request<'_>| capability::authorize(&wider, root.public(), request);
    let key = slot(1);

    assert_eq!(
        check(&request(
            Operation::Read,
            OTHER_PLAYER_SLOT,
            "paid_currency"
        )),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&Request {
            now: now() + 2 * HOUR,
            ..request(Operation::Read, &key, "count")
        }),
        Err(Denied::Expired)
    );
}

/// A holder who writes the issuer's facts into a block of their own gets
/// nothing more: the policy reads only the issuer's block.
#[test]
fn facts_appended_by_a_holder_grant_nothing() {
    let root = KeyPair::new();
    let read_only = issue(
        &root,
        &Grant {
            operations: &[Operation::Read],
            ..grant()
        },
    );
    let forged = BlockBuilder::new()
        .code_with_params(
            r#"
            key_prefix("player/");
            column_allowed("paid_currency");
            operation_allowed("write");
            purpose("analytics");
            "#,
            HashMap::new(),
            HashMap::new(),
        )
        .expect("parses");
    let widened = Biscuit::from(&read_only, root.public())
        .and_then(|token| token.append(forged))
        .and_then(|token| token.to_vec())
        .expect("appends");
    let check = |request: &Request<'_>| capability::authorize(&widened, root.public(), request);
    let key = slot(1);

    assert_eq!(
        check(&request(Operation::Write, &key, "count")),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&request(Operation::Read, OTHER_PLAYER_SLOT, "count")),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&request(Operation::Read, &key, "paid_currency")),
        Err(Denied::OutOfScope)
    );
    assert_eq!(
        check(&Request {
            purpose: "analytics",
            ..request(Operation::Read, &key, "count")
        }),
        Err(Denied::OutOfScope)
    );
    assert_eq!(check(&request(Operation::Read, &key, "count")), Ok(()));
}

#[test]
fn a_token_from_another_root_or_altered_is_invalid() {
    let root = KeyPair::new();
    let foreign = issue(&KeyPair::new(), &grant());
    let mut altered = issue(&root, &grant());
    let middle = altered.len() / 2;
    altered[middle] ^= 1;
    let key = slot(1);
    let read = request(Operation::Read, &key, "count");

    assert_eq!(
        capability::authorize(&foreign, root.public(), &read),
        Err(Denied::Invalid)
    );
    assert_eq!(
        capability::authorize(&altered, root.public(), &read),
        Err(Denied::Invalid)
    );
    assert_eq!(
        capability::authorize(b"not a token", root.public(), &read),
        Err(Denied::Invalid)
    );
}

#[test]
fn writes_land_only_on_the_tokens_branch() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let key = slot(1);
    let write_to = |branch| {
        capability::authorize(
            &token,
            root.public(),
            &Request {
                branch,
                ..request(Operation::Write, &key, "count")
            },
        )
    };
    let read_from = |branch| {
        capability::authorize(
            &token,
            root.public(),
            &Request {
                branch,
                ..request(Operation::Read, &key, "count")
            },
        )
    };

    assert_eq!(write_to(BRANCH), Ok(()));
    assert_eq!(write_to("agent/other"), Err(Denied::OutOfScope));
    assert_eq!(write_to(MAIN_BRANCH), Err(Denied::MainBranch));
    // Reading is not tied to the token's branch: an agent reads live data.
    assert_eq!(read_from("agent/other"), Ok(()));
}

#[test]
fn a_token_cannot_be_issued_for_writes_to_main() {
    let root = KeyPair::new();

    let issued = capability::issue(
        &root,
        &Grant {
            branch: MAIN_BRANCH,
            ..grant()
        },
    );

    assert!(issued.is_err());
}

#[test]
fn the_holder_is_read_from_the_issuers_block() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let narrowed = capability::narrow(
        &token,
        root.public(),
        &Narrowing {
            operation: Some(Operation::Read),
            ..Narrowing::default()
        },
    )
    .expect("narrows");
    let expected = Holder {
        subject: SUBJECT.to_owned(),
        branch: BRANCH.to_owned(),
        purpose: PURPOSE.to_owned(),
        key_prefix: grant().key_prefix.to_owned(),
        columns: {
            let mut columns: Vec<String> =
                grant().columns.iter().map(|c| (*c).to_owned()).collect();
            columns.sort();
            columns
        },
    };

    assert_eq!(
        capability::holder(&token, root.public()),
        Ok(expected.clone())
    );
    assert_eq!(capability::holder(&narrowed, root.public()), Ok(expected));
    assert_eq!(
        capability::holder(&issue(&KeyPair::new(), &grant()), root.public()),
        Err(Denied::Invalid)
    );
}

/// A holder who claims to be someone else, or to write somewhere else, in a
/// block of their own is still who the issuer said, writing where the
/// issuer said.
#[test]
fn a_holder_cannot_rename_themselves_or_move_their_branch() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());
    let forged = BlockBuilder::new()
        .code_with_params(
            r#"
            subject("ops:lead");
            write_branch("agent/other");
            "#,
            HashMap::new(),
            HashMap::new(),
        )
        .expect("parses");
    let renamed = Biscuit::from(&token, root.public())
        .and_then(|token| token.append(forged))
        .and_then(|token| token.to_vec())
        .expect("appends");
    let key = slot(1);

    let holder = capability::holder(&renamed, root.public()).expect("reads");
    let write = capability::authorize(
        &renamed,
        root.public(),
        &Request {
            branch: "agent/other",
            ..request(Operation::Write, &key, "count")
        },
    );

    assert_eq!(holder.subject, SUBJECT);
    assert_eq!(holder.branch, BRANCH);
    assert_eq!(write, Err(Denied::OutOfScope));
}

fn approver_grant() -> Grant<'static> {
    Grant {
        operations: &[Operation::Read, Operation::Approve],
        subject: "person:lead",
        ..grant()
    }
}

/// Approving a change into `main`, one changed key at a time.
fn approval(key: &str) -> Request<'_> {
    Request {
        operation: Operation::Approve,
        branch: MAIN_BRANCH,
        key,
        column: "",
        purpose: PURPOSE,
        now: now(),
    }
}

#[test]
fn a_token_that_may_approve_approves_changes_to_its_keys() {
    let root = KeyPair::new();
    let token = issue(&root, &approver_grant());

    assert_eq!(
        capability::authorize(&token, root.public(), &approval(&slot(1))),
        Ok(())
    );
    assert_eq!(
        capability::authorize(&token, root.public(), &approval(OTHER_PLAYER_SLOT)),
        Err(Denied::OutOfScope)
    );
}

#[test]
fn an_agents_token_cannot_approve() {
    let root = KeyPair::new();
    let token = issue(&root, &grant());

    assert_eq!(
        capability::authorize(&token, root.public(), &approval(&slot(1))),
        Err(Denied::OutOfScope)
    );
}

#[test]
fn a_token_that_may_approve_cannot_write() {
    let root = KeyPair::new();
    let token = issue(&root, &approver_grant());

    assert_eq!(
        capability::authorize(
            &token,
            root.public(),
            &request(Operation::Write, &slot(1), "item")
        ),
        Err(Denied::OutOfScope)
    );
}

#[test]
fn a_token_that_both_writes_and_approves_is_not_issued() {
    let root = KeyPair::new();
    let both = Grant {
        operations: &[Operation::Write, Operation::Approve],
        ..grant()
    };

    assert!(matches!(
        capability::issue(&root, &both),
        Err(capability::TokenError::WritesAndApproves)
    ));
}

/// A token verified once answers every request as checking it from its
/// bytes each time does: inside and outside its keys and columns, narrowed
/// by its holder, expired, and for writes to main.
#[test]
fn a_token_verified_once_answers_as_one_checked_every_time() {
    let root = KeyPair::new();
    let (first, second) = (slot(1), slot(2));
    let issued = issue(&root, &grant());
    let narrowed = capability::narrow(
        &issued,
        root.public(),
        &Narrowing {
            operation: Some(Operation::Read),
            key_prefix: Some(&first),
            ..Narrowing::default()
        },
    )
    .expect("narrows");
    let later = Request {
        now: now() + 2 * HOUR,
        ..request(Operation::Read, &first, "count")
    };
    let to_main = Request {
        branch: MAIN_BRANCH,
        ..request(Operation::Write, &first, "item")
    };
    let requests = [
        request(Operation::Read, &first, "count"),
        request(Operation::Write, &first, "item"),
        request(Operation::Read, &second, "count"),
        request(Operation::Read, OTHER_PLAYER_SLOT, "count"),
        request(Operation::Read, &first, "secret"),
        later,
        to_main,
        approval(&first),
    ];

    for token in [&issued, &narrowed] {
        let verified = capability::Verified::new(token, root.public()).expect("verifies");
        for request in &requests {
            assert_eq!(
                verified.authorize(request),
                capability::authorize(token, root.public(), request),
                "{request:?}"
            );
        }
        assert_eq!(
            verified.holder().expect("has a holder"),
            capability::holder(token, root.public()).expect("has a holder")
        );
    }
}

#[test]
fn a_token_another_root_signed_does_not_verify() {
    let token = issue(&KeyPair::new(), &grant());

    let refused = capability::Verified::new(&token, KeyPair::new().public());

    assert_eq!(refused.err(), Some(Denied::Invalid));
}
