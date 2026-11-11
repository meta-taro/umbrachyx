//! Who is on the other end of a connection. `AUTH` with a capability token
//! makes it the token's holder: the author of its writes, which the token
//! bounds. Without it, a connection reads and, unless the server requires
//! authentication, writes as `unauthenticated`.

use std::time::SystemTime;

use biscuit_auth::PublicKey;

use super::resp::Reply;
use crate::capability::{self, Denied, Holder, Operation, Request, Verified};
use crate::store::Provenance;

/// The author recorded for a write from a connection without `AUTH`.
pub const UNAUTHENTICATED: &str = "unauthenticated";

pub(super) struct Session {
    /// The token, its signatures checked once, when it was given.
    token: Option<(Verified, Holder)>,
}

impl Session {
    pub(super) fn new() -> Self {
        Self { token: None }
    }

    /// `AUTH <token>` or `AUTH <user> <token>`, as Redis takes a password;
    /// the user name is not used. A refused token leaves the connection as
    /// it was.
    pub(super) fn authenticate(&mut self, root: Option<PublicKey>, args: &[Vec<u8>]) -> Reply {
        let Some(root) = root else {
            return Reply::Error("ERR AUTH needs the server started with --public-key".to_owned());
        };
        let text = match args {
            [token] | [_, token] => String::from_utf8_lossy(token),
            _ => {
                return Reply::Error("ERR wrong number of arguments for 'auth' command".to_owned());
            }
        };
        let token = match capability::from_text(text.trim()) {
            Ok(token) => token,
            Err(_) => return wrong_pass(),
        };
        let verified = match Verified::new(&token, root) {
            Ok(verified) => verified,
            Err(_) => return wrong_pass(),
        };
        match verified.holder() {
            Ok(holder) => {
                self.token = Some((verified, holder));
                Reply::Simple("OK")
            }
            Err(_) => wrong_pass(),
        }
    }

    /// Whether this connection may write `key` on `branch`: a token decides
    /// for an authenticated one; otherwise only whether the server takes
    /// writes without `AUTH`.
    pub(super) fn may_write(
        &self,
        branch: &str,
        key: &[u8],
        require_auth: bool,
    ) -> Result<(), Reply> {
        let Some((token, holder)) = &self.token else {
            return if require_auth {
                Err(Reply::Error("NOAUTH Authentication required.".to_owned()))
            } else {
                Ok(())
            };
        };
        let key = std::str::from_utf8(key).map_err(|_| no_permission(Denied::OutOfScope))?;
        let column = key.rsplit('/').next().unwrap_or(key);
        token
            .authorize(&Request {
                operation: Operation::Write,
                branch,
                key,
                column,
                purpose: &holder.purpose,
                now: SystemTime::now(),
            })
            .map_err(no_permission)
    }

    /// The token this connection authenticated with, and its holder.
    pub(super) fn token(&self) -> Option<(&Verified, &Holder)> {
        self.token.as_ref().map(|(token, holder)| (token, holder))
    }

    /// Who and why for a write from this connection.
    pub(super) fn provenance(&self, command: &str) -> Provenance {
        let who = match &self.token {
            Some((_, holder)) => holder.subject.clone(),
            None => UNAUTHENTICATED.to_owned(),
        };
        Provenance {
            who,
            why: command.to_owned(),
            approved_by: None,
        }
    }
}

fn wrong_pass() -> Reply {
    Reply::Error("WRONGPASS the token is not valid for this database".to_owned())
}

fn no_permission(denied: Denied) -> Reply {
    Reply::Error(format!("NOPERM {denied}"))
}
