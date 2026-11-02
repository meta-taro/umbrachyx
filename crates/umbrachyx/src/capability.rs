//! Capability tokens: what a caller may touch, carried by the caller and
//! checked by the database on every request, before any data is read.
//!
//! A token names which keys (by prefix), which columns, which operations,
//! for what purpose and until when. Writes are refused on `main`, so an
//! agent's writes always land on a branch. Whoever holds a token can narrow
//! it offline by appending a block of checks; nobody can widen it, because
//! the database's policy reads only the facts in the first block, which the
//! issuer signed. The token format and its checks are biscuit's; nothing
//! cryptographic is written here.
//!
//! This is a prototype. Keys are strings, because biscuit can test a string
//! for a prefix but not a byte string.

use std::collections::HashMap;
use std::fmt;
use std::time::SystemTime;

use biscuit_auth::builder::Term;
use biscuit_auth::error::{FailedCheck, Logic, Token};
use biscuit_auth::{AuthorizerBuilder, Biscuit, BiscuitBuilder, BlockBuilder, KeyPair, PublicKey};

/// The branch that holds live data, which tokens never write to.
pub const MAIN_BRANCH: &str = "main";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
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
pub struct TokenError(Token);

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "capability token: {}", self.0)
    }
}

impl std::error::Error for TokenError {}

impl From<Token> for TokenError {
    fn from(error: Token) -> Self {
        Self(error)
    }
}

/// Every check on the time names this fact and every check on the branch
/// names the other, which is how the reason for a refusal is told apart.
const TIME_FACT: &str = "time(";
const BRANCH_FACT: &str = "branch(";

pub fn issue(root: &KeyPair, grant: &Grant<'_>) -> Result<Vec<u8>, TokenError> {
    let mut builder = BiscuitBuilder::new().code_with_params(
        r#"
        key_prefix({prefix});
        purpose({purpose});
        check if time($t), $t <= {expires};
        check if operation("read") or branch($b), $b != {main};
        "#,
        params([
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

/// Decides one request. Runs at the database's boundary, before the request
/// reaches the log or the tree.
pub fn authorize(token: &[u8], root: PublicKey, request: &Request<'_>) -> Result<(), Denied> {
    let token = Biscuit::from(token, root).map_err(|_| Denied::Invalid)?;
    // The policy is evaluated over the authorizer's facts and the first
    // block's only, so facts a holder appends cannot satisfy it.
    let mut authorizer = AuthorizerBuilder::new()
        .code_with_params(
            r#"
            operation({operation});
            branch({branch});
            key({key});
            column({column});
            declared_purpose({purpose});
            time({now});
            allow if operation($o), operation_allowed($o),
                     key($k), key_prefix($p), $k.starts_with($p),
                     column($c), column_allowed($c),
                     declared_purpose($u), purpose($u);
            "#,
            params([
                ("operation", request.operation.name().into()),
                ("branch", request.branch.into()),
                ("key", request.key.into()),
                ("column", request.column.into()),
                ("purpose", request.purpose.into()),
                ("now", request.now.into()),
            ]),
            HashMap::new(),
        )
        .and_then(|builder| builder.build(&token))
        .map_err(|_| Denied::Invalid)?;
    match authorizer.authorize() {
        Ok(_) => Ok(()),
        Err(Token::FailedLogic(
            Logic::Unauthorized { checks, .. } | Logic::NoMatchingPolicy { checks },
        )) => Err(reason(&checks)),
        // Anything else, such as reaching the evaluation limits, is refused
        // too: a request is allowed only by a policy that matched.
        Err(_) => Err(Denied::Invalid),
    }
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
