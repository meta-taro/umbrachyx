//! Capability tokens: what a caller may touch, carried by the caller and
//! checked by the database on every request, before any data is read.
//!
//! A token names who holds it, which keys (by prefix), which columns, which
//! operations, for what purpose, until when, and the one branch its writes
//! go to. That branch is never `main`, so an agent's writes always land on a
//! branch, and the holder cannot choose another. Whoever holds a token can narrow
//! it offline by appending a block of checks; nobody can widen it, because
//! the database's policy reads only the facts in the first block, which the
//! issuer signed. The token format and its checks are biscuit's; nothing
//! cryptographic is written here.
//!
//! This is a prototype. Keys are strings, because biscuit can test a string
//! for a prefix but not a byte string.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use biscuit_auth::builder::{Term, date, fact, string};
use biscuit_auth::error::{FailedCheck, Logic, Token};
use biscuit_auth::{
    AuthorizerBuilder, AuthorizerLimits, Biscuit, BiscuitBuilder, BlockBuilder, KeyPair, PublicKey,
    UnverifiedBiscuit,
};

/// The branch that holds live data, which tokens never write to.
pub use crate::store::MAIN_BRANCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
    /// Merging a change into `main`, one changed key at a time. Never in
    /// the same token as `Write`, so whoever writes a change cannot also
    /// pass it.
    Approve,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Approve => "approve",
        }
    }
}

/// What the issuer grants.
#[derive(Debug, Clone)]
pub struct Grant<'a> {
    pub operations: &'a [Operation],
    pub key_prefix: &'a str,
    pub columns: &'a [&'a str],
    pub purpose: &'a str,
    pub expires: SystemTime,
    /// Who holds the token, recorded as the author of its commits.
    pub subject: &'a str,
    /// The only branch the token writes to. Never `main`.
    pub branch: &'a str,
}

/// Who a token was issued to and where its writes go, as the issuer signed
/// them. Blocks a holder appends cannot change these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub subject: String,
    pub branch: String,
    pub purpose: String,
    /// The keys the issuer granted. A holder may have narrowed it since, so
    /// it bounds what the token reads; it is not a permission by itself.
    pub key_prefix: String,
    /// The columns the issuer granted, sorted; bounds as `key_prefix` does.
    pub columns: Vec<String>,
}

/// What a holder takes away before handing a token on. Each field left
/// `None` keeps what the token already allows.
#[derive(Debug, Clone, Default)]
pub struct Narrowing<'a> {
    pub operation: Option<Operation>,
    pub key_prefix: Option<&'a str>,
    pub column: Option<&'a str>,
    pub expires: Option<SystemTime>,
}

/// One read or write of one column of one record.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub operation: Operation,
    pub branch: &'a str,
    pub key: &'a str,
    pub column: &'a str,
    pub purpose: &'a str,
    pub now: SystemTime,
}

/// Why a request was refused. The caller is told only which of these it
/// was, not which fact failed, so a refusal reveals nothing more about what
/// the token would have allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// Not a token signed by this database's root key, or damaged.
    Invalid,
    Expired,
    /// A write to the main branch. Writes go to a branch and are merged.
    MainBranch,
    /// The operation, key, column or purpose is outside the token.
    OutOfScope,
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "the token is not valid",
            Self::Expired => "the token has expired",
            Self::MainBranch => "writes must go to a branch, not main",
            Self::OutOfScope => "the request is outside what the token allows",
        })
    }
}

impl std::error::Error for Denied {}

/// A token could not be built or read.
#[derive(Debug)]
pub enum TokenError {
    Biscuit(Token),
    /// A token whose writes would go to `main` is never issued.
    WritesToMain,
    /// A token that writes never also approves.
    WritesAndApproves,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Biscuit(error) => write!(f, "capability token: {error}"),
            Self::WritesToMain => write!(
                f,
                "capability token: writes must go to a branch, not {MAIN_BRANCH}"
            ),
            Self::WritesAndApproves => write!(
                f,
                "capability token: a token that writes cannot also approve"
            ),
        }
    }
}

impl std::error::Error for TokenError {}

impl From<Token> for TokenError {
    fn from(error: Token) -> Self {
        Self::Biscuit(error)
    }
}

/// Limits on evaluating one request. The fact and iteration limits are
/// biscuit's defaults and do not depend on the machine. The time limit only
/// stops a token built to be expensive: biscuit's default of 1 ms is
/// exceeded by ordinary tokens on a loaded machine, which refused valid
/// requests at random.
const LIMITS: AuthorizerLimits = AuthorizerLimits {
    max_facts: 1_000,
    max_iterations: 100,
    max_time: Duration::from_millis(50),
};

/// Every check on the time names this fact and every check on the branch
/// names the other, which is how the reason for a refusal is told apart.
const TIME_FACT: &str = "time(";
const BRANCH_FACT: &str = "branch(";

pub fn issue(root: &KeyPair, grant: &Grant<'_>) -> Result<Vec<u8>, TokenError> {
    if grant.branch == MAIN_BRANCH {
        return Err(TokenError::WritesToMain);
    }
    if grant.operations.contains(&Operation::Write)
        && grant.operations.contains(&Operation::Approve)
    {
        return Err(TokenError::WritesAndApproves);
    }
    // The check on main stays even though the branch is never main: it is
    // what tells a write to main apart from a write to another branch.
    let mut builder = BiscuitBuilder::new().code_with_params(
        r#"
        subject({subject});
        write_branch({branch});
        key_prefix({prefix});
        purpose({purpose});
        check if time($t), $t <= {expires};
        check if operation("read") or operation("approve") or branch($b), $b != {main};
        "#,
        params([
            ("subject", grant.subject.into()),
            ("branch", grant.branch.into()),
            ("prefix", grant.key_prefix.into()),
            ("purpose", grant.purpose.into()),
            ("expires", grant.expires.into()),
            ("main", MAIN_BRANCH.into()),
        ]),
        HashMap::new(),
    )?;
    for operation in grant.operations {
        builder = builder.code_with_params(
            "operation_allowed({operation});",
            params([("operation", operation.name().into())]),
            HashMap::new(),
        )?;
    }
    for column in grant.columns {
        builder = builder.code_with_params(
            "column_allowed({column});",
            params([("column", (*column).into())]),
            HashMap::new(),
        )?;
    }
    Ok(builder.build(root)?.to_vec()?)
}

/// Appends checks to `token`. The result allows at most what `token` did.
pub fn narrow(
    token: &[u8],
    root: PublicKey,
    narrowing: &Narrowing<'_>,
) -> Result<Vec<u8>, TokenError> {
    let mut block = BlockBuilder::new();
    if let Some(operation) = narrowing.operation {
        block = block.code_with_params(
            "check if operation({operation});",
            params([("operation", operation.name().into())]),
            HashMap::new(),
        )?;
    }
    if let Some(prefix) = narrowing.key_prefix {
        block = block.code_with_params(
            "check if key($k), $k.starts_with({prefix});",
            params([("prefix", prefix.into())]),
            HashMap::new(),
        )?;
    }
    if let Some(column) = narrowing.column {
        block = block.code_with_params(
            "check if column({column});",
            params([("column", column.into())]),
            HashMap::new(),
        )?;
    }
    if let Some(expires) = narrowing.expires {
        block = block.code_with_params(
            "check if time($t), $t <= {expires};",
            params([("expires", expires.into())]),
            HashMap::new(),
        )?;
    }
    Ok(Biscuit::from(token, root)?.append(block)?.to_vec()?)
}

/// A token as text, for a file or a pipe. Base64, as biscuit's own tools
/// write it.
pub fn to_text(token: &[u8]) -> Result<String, TokenError> {
    Ok(UnverifiedBiscuit::from(token)?.to_base64()?)
}

/// The token in `text`, surrounding whitespace ignored. Its signatures are
/// checked later, against the root key, by whatever uses it.
pub fn from_text(text: &str) -> Result<Vec<u8>, TokenError> {
    Ok(UnverifiedBiscuit::from_base64(text.trim())?.to_vec()?)
}

/// What allows a request. What a request states is given as facts
/// beside it, see [`Verified::authorize`].
const POLICY: &str = r#"
            allow if operation("read"), operation_allowed("read"),
                     key($k), key_prefix($p), $k.starts_with($p),
                     column($c), column_allowed($c),
                     declared_purpose($u), purpose($u);
            allow if operation("write"), operation_allowed("write"),
                     branch($b), write_branch($b),
                     key($k), key_prefix($p), $k.starts_with($p),
                     column($c), column_allowed($c),
                     declared_purpose($u), purpose($u);
            allow if operation("approve"), operation_allowed("approve"),
                     branch({main}),
                     key($k), key_prefix($p), $k.starts_with($p),
                     declared_purpose($u), purpose($u);
            "#;

/// A token whose signatures have been checked against the root key, kept
/// so that the check is made once. Verifying the signatures is most of
/// what deciding a request from a token's bytes costs, and a connection
/// decides one for every write. Each request is still decided by biscuit,
/// over all the token's blocks: only the signatures are not checked again,
/// and they cannot change.
pub struct Verified(Biscuit);

impl Verified {
    /// Refuses a token not signed by `root`, or damaged.
    pub fn new(token: &[u8], root: PublicKey) -> Result<Self, Denied> {
        Biscuit::from(token, root)
            .map(Self)
            .map_err(|_| Denied::Invalid)
    }

    /// Who holds the token and where its writes go. It does not decide any
    /// request: a token that has expired still has a holder.
    pub fn holder(&self) -> Result<Holder, Denied> {
        // A query sees the authorizer's facts and the first block's only,
        // as the policy does, so a holder's own blocks cannot change the
        // answer.
        let mut authorizer = AuthorizerBuilder::new()
            .set_limits(LIMITS)
            .build(&self.0)
            .map_err(|_| Denied::Invalid)?;
        let (subject, branch, purpose, key_prefix) = authorizer
            .query_exactly_one(
                "holder($s, $b, $p, $k) <- subject($s), write_branch($b), purpose($p), key_prefix($k)",
            )
            .map_err(|_| Denied::Invalid)?;
        let columns: Vec<(String,)> = authorizer
            .query("column($c) <- column_allowed($c)")
            .map_err(|_| Denied::Invalid)?;
        let mut columns: Vec<String> = columns.into_iter().map(|(column,)| column).collect();
        columns.sort();
        Ok(Holder {
            subject,
            branch,
            purpose,
            key_prefix,
            columns,
        })
    }

    /// Decides one request. Runs at the database's boundary, before the
    /// request reaches the log or the tree.
    pub fn authorize(&self, request: &Request<'_>) -> Result<(), Denied> {
        // The policy is evaluated over the authorizer's facts and the first
        // block's only, so facts a holder appends cannot satisfy it.
        let stated = [
            ("operation", string(request.operation.name())),
            ("branch", string(request.branch)),
            ("key", string(request.key)),
            ("column", string(request.column)),
            ("declared_purpose", string(request.purpose)),
            ("time", date(&request.now)),
        ];
        let mut authorizer = stated
            .into_iter()
            .try_fold(policy()?, |builder, (name, term)| {
                builder.fact(fact(name, &[term]))
            })
            .and_then(|builder| builder.build(&self.0))
            .map_err(|_| Denied::Invalid)?;
        match authorizer.authorize() {
            Ok(_) => Ok(()),
            Err(Token::FailedLogic(
                Logic::Unauthorized { checks, .. } | Logic::NoMatchingPolicy { checks },
            )) => Err(reason(&checks)),
            // Anything else, such as reaching the evaluation limits, is
            // refused as invalid: a request is allowed only by a policy
            // that matched.
            Err(_) => Err(Denied::Invalid),
        }
    }
}

/// The policy, read from its text once: reading it for every request was
/// most of what deciding one cost. Each request starts from a copy and
/// adds what it states.
fn policy() -> Result<AuthorizerBuilder, Denied> {
    static POLICY_READ: OnceLock<Option<AuthorizerBuilder>> = OnceLock::new();
    POLICY_READ
        .get_or_init(|| {
            AuthorizerBuilder::new()
                .code_with_params(
                    POLICY,
                    params([("main", MAIN_BRANCH.into())]),
                    HashMap::new(),
                )
                .map(|builder| builder.set_limits(LIMITS))
                .ok()
        })
        .clone()
        .ok_or(Denied::Invalid)
}

/// Reads who holds `token` and where its writes go, checking its
/// signatures first. See [`Verified::holder`].
pub fn holder(token: &[u8], root: PublicKey) -> Result<Holder, Denied> {
    Verified::new(token, root)?.holder()
}

/// Decides one request from a token's bytes, checking its signatures
/// first. See [`Verified::authorize`].
pub fn authorize(token: &[u8], root: PublicKey, request: &Request<'_>) -> Result<(), Denied> {
    Verified::new(token, root)?.authorize(request)
}

/// An expired token is reported as expired even when the request is also
/// out of scope: renewing is the first thing the caller has to do.
fn reason(checks: &[FailedCheck]) -> Denied {
    let rules: Vec<&str> = checks
        .iter()
        .map(|check| match check {
            FailedCheck::Block(check) => check.rule.as_str(),
            FailedCheck::Authorizer(check) => check.rule.as_str(),
        })
        .collect();
    if rules.iter().any(|rule| rule.contains(TIME_FACT)) {
        Denied::Expired
    } else if rules.iter().any(|rule| rule.contains(BRANCH_FACT)) {
        Denied::MainBranch
    } else {
        Denied::OutOfScope
    }
}

fn params<const N: usize>(pairs: [(&str, Term); N]) -> HashMap<String, Term> {
    pairs
        .into_iter()
        .map(|(name, term)| (name.to_owned(), term))
        .collect()
}
