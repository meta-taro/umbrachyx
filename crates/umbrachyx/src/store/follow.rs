//! How a replica follows its primary: what it says it holds, what it is
//! sent, and what either does when the two do not fit.
//!
//! The replica asks. It says which log it holds, how many records of it,
//! and the chain over them; the primary answers with the records that
//! come after, or says why it sends none. Nothing is pushed, so a primary
//! keeps no list of replicas and owes a slow one nothing, and what a
//! replica holds is what it last said: an ask is also the news that
//! everything before it is on the replica's disk.
//!
//! The two meet through [`Primary`], which is a connection in a server
//! and a function call in a test, so the same loop is run against a
//! network that loses, repeats, delays and cuts short what it carries.
//!
//! A replica starts as a restored backup of its primary. That gives it
//! the log's name, without which a primary sends nothing, and the keys
//! the values were sealed under by then.
//!
//! The key of a subject first written later is not among the records:
//! the log never holds a key. A replica that is sent records sealed
//! under keys it lacks asks for those keys by their fingerprints, over a
//! link that carries keys ([`Primary::keys`]; most carry none, and then
//! the replica stops where it is). It keeps a key only if it is the one
//! the records name, and has it on its disk before the records.

use std::fmt;

use super::{Clock, Store, StoreError};
use crate::keys::{Fingerprint, Key, fingerprint};
use crate::log::{Chain, LogError, LogId, Storage};

/// The most a primary sends in one answer, whatever is asked for.
pub const MAX_SENT: usize = 1024 * 1024;
/// How much of a log's chain is worked out in one turn.
const CHAIN_STEP: u64 = 1024 * 1024;

/// What a replica holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The name of the log, which every copy of it keeps.
    pub log: LogId,
    pub records: u64,
    /// The chain over those records.
    pub chain: Chain,
}

/// A replica asking for the records after the ones it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ask {
    pub held: Held,
    /// As many whole records as fit in this, and one if none does.
    pub max_bytes: usize,
}

/// A primary's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// How many records the replica said it holds: the frames start
    /// there. A replica that holds another number by the time the answer
    /// arrives leaves it.
    pub after: u64,
    /// How many records the primary holds.
    pub records: u64,
    /// Whole records, as the primary's log holds them. None if the
    /// replica holds them all.
    pub frames: Vec<u8>,
}

/// Why a primary sends a replica nothing. None of these mends itself: the
/// replica stops asking and says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The replica holds another log.
    AnotherLog,
    /// The primary holds fewer records than the replica: this many.
    Fewer { records: u64 },
    /// The same log, with other records in it: one of the two was written
    /// to apart from the other.
    Diverged,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnotherLog => write!(
                f,
                "the primary holds another log; a replica starts as a restored backup of its \
                 primary"
            ),
            Self::Fewer { records } => write!(
                f,
                "the primary holds {records} records, fewer than this replica: it is not the \
                 store this one followed, or it went back to an older copy"
            ),
            Self::Diverged => write!(
                f,
                "the primary's log and this replica's hold different records: one of them was \
                 written to apart from the other"
            ),
        }
    }
}

/// What a primary says of a key a replica asked for. Each says which
/// key it is about, so one that arrives in place of another is seen to
/// be: a key by what it is, the others by the fingerprint asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyAnswer {
    Key(Key),
    /// Its log holds the erasure that destroyed the key.
    Erased(Fingerprint),
    /// It never held such a key.
    Unknown(Fingerprint),
}

impl KeyAnswer {
    /// The fingerprint of the key this is about.
    pub fn fingerprint(&self) -> Fingerprint {
        match self {
            Self::Key(key) => fingerprint(key),
            Self::Erased(about) | Self::Unknown(about) => *about,
        }
    }
}

/// Never the key itself.
impl fmt::Debug for KeyAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Key(_) => "Key(..)",
            Self::Erased(_) => "Erased",
            Self::Unknown(_) => "Unknown",
        })
    }
}

/// A log whose chain is worked out a part at a time, so that asking for
/// it holds nobody up for as long as hashing a whole log takes.
pub trait Chained {
    /// Works out some more and returns whether any is left.
    fn chain_step(&mut self) -> Result<bool, StoreError>;
}

/// The store a replica is fed from.
pub trait Feed: Chained {
    /// The answer to `ask`, in one turn.
    fn sent(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, StoreError>;
    /// What this store says of the key with each of these fingerprints,
    /// in the same order. Only whoever may read every value is to be
    /// answered.
    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Vec<KeyAnswer>, StoreError>;
}

/// The store a replica keeps.
pub trait Follower: Chained {
    fn held(&mut self) -> Result<Held, StoreError>;
    /// Takes in what was sent, if it starts where this store's log ends,
    /// and returns how many records the store holds then.
    fn take_in(&mut self, sent: &Sent) -> Result<u64, StoreError>;
    /// The fingerprints of the keys this store lacks to take in `sent`.
    /// None if it would leave `sent` anyway.
    fn lacks(&mut self, sent: &Sent) -> Result<Vec<Fingerprint>, StoreError>;
    /// Keeps those of `keys` that `sent` needs and this store lacks,
    /// durable before this returns.
    fn take_keys(&mut self, sent: &Sent, keys: &[Key]) -> Result<(), StoreError>;
}

/// The way to a primary.
pub trait Primary {
    /// Asks, and returns the answer or why none came. Having no answer
    /// is not a refusal: the replica asks again.
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String>;

    /// Asks for the keys with these fingerprints. `None` if keys do not
    /// travel this way, which is so unless the way is one nobody else
    /// can read or write on.
    fn keys(&mut self, _wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        Ok(None)
    }
}

/// A store in the same process, asked directly, keys included: for where
/// nothing lies between the two stores.
pub struct WithKeys<'a, F: Feed>(pub &'a mut F);

impl<F: Feed> Primary for WithKeys<'_, F> {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        self.0.ask(ask)
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        match self.0.keys(wanted) {
            Ok(answers) => Ok(Some(answers)),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Answers a replica from `feed`.
pub fn answer(
    feed: &mut (impl Feed + ?Sized),
    ask: &Ask,
) -> Result<Result<Sent, Refusal>, StoreError> {
    while feed.chain_step()? {}
    feed.sent(ask)
}

/// A store in the same process is asked directly. Keys are not handed
/// over this way; see [`WithKeys`].
impl<F: Feed> Primary for F {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        answer(self, ask).map_err(|error| error.to_string())
    }
}

/// Why a replica took nothing in.
#[derive(Debug)]
pub enum FollowError {
    /// No answer came. Worth asking again.
    Link(String),
    /// The primary will not feed this replica.
    Refused(Refusal),
    /// This replica could not read its own log, or could not take in what
    /// it was sent: it lacks a key, or its log refused the write.
    Store(StoreError),
}

impl fmt::Display for FollowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(why) => write!(f, "no answer from the primary: {why}"),
            Self::Refused(refusal) => write!(f, "{refusal}"),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FollowError {}

impl From<StoreError> for FollowError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// How far a replica is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Records the replica holds.
    pub held: u64,
    /// Records the primary held when it answered.
    pub primary: u64,
}

impl Progress {
    pub fn is_caught_up(&self) -> bool {
        self.held >= self.primary
    }
}

/// Asks `primary` once for what follows the records `follower` holds, at
/// most `max_bytes` of them, and takes it in.
pub fn follow_once(
    primary: &mut (impl Primary + ?Sized),
    follower: &mut (impl Follower + ?Sized),
    max_bytes: usize,
) -> Result<Progress, FollowError> {
    while follower.chain_step()? {}
    let held = follower.held()?;
    let sent = primary
        .ask(&Ask { held, max_bytes })
        .map_err(FollowError::Link)?
        .map_err(FollowError::Refused)?;
    fetch_keys(primary, follower, &sent)?;
    let now = follower.take_in(&sent)?;
    Ok(Progress {
        held: now,
        primary: sent.records,
    })
}

/// Asks `primary` for the keys `follower` lacks to take in `sent`, and
/// has it keep them. Over a link that carries no keys this does nothing,
/// and taking in `sent` then says which keys are missing.
fn fetch_keys(
    primary: &mut (impl Primary + ?Sized),
    follower: &mut (impl Follower + ?Sized),
    sent: &Sent,
) -> Result<(), FollowError> {
    let wanted = follower.lacks(sent)?;
    if wanted.is_empty() {
        return Ok(());
    }
    let Some(answers) = primary.keys(&wanted).map_err(FollowError::Link)? else {
        return Ok(());
    };
    // An answer that does not fit the ask did not come from the store
    // that was asked, or not whole: ask again, and keep nothing of it.
    if answers.len() != wanted.len() {
        return Err(FollowError::Link(format!(
            "asked for {} key(s) and was answered for {}",
            wanted.len(),
            answers.len()
        )));
    }
    let mut keys = Vec::new();
    let mut erased = 0;
    for (wanted, answer) in wanted.iter().zip(&answers) {
        if answer.fingerprint() != *wanted {
            return Err(FollowError::Link(
                "was answered about a key other than the one asked for".to_owned(),
            ));
        }
        match answer {
            KeyAnswer::Key(key) => keys.push(*key),
            KeyAnswer::Erased(_) => erased += 1,
            // Taking in what was sent says how many are missing.
            KeyAnswer::Unknown(_) => {}
        }
    }
    if erased > 0 {
        return Err(StoreError::KeysErased(erased).into());
    }
    follower.take_keys(sent, &keys)?;
    Ok(())
}

impl<S: Storage, C: Clock> Chained for Store<S, C> {
    fn chain_step(&mut self) -> Result<bool, StoreError> {
        self.log
            .chain_step(CHAIN_STEP)
            .map_err(|error| LogError::from(error).into())
    }
}

impl<S: Storage, C: Clock> Feed for Store<S, C> {
    fn sent(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, StoreError> {
        let held = &ask.held;
        if held.log != self.log.id() {
            return Ok(Err(Refusal::AnotherLog));
        }
        let records = self.log.records();
        if held.records > records {
            return Ok(Err(Refusal::Fewer { records }));
        }
        if self.log_chain(held.records)? != Some(held.chain) {
            return Ok(Err(Refusal::Diverged));
        }
        let frames = self.log_after(held.records, ask.max_bytes.min(MAX_SENT))?;
        Ok(Ok(Sent {
            after: held.records,
            records,
            frames,
        }))
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Vec<KeyAnswer>, StoreError> {
        Ok(self.keys_wanted(wanted))
    }
}

impl<S: Storage, C: Clock> Follower for Store<S, C> {
    fn held(&mut self) -> Result<Held, StoreError> {
        let records = self.log.records();
        let chain = self.log_chain(records)?.ok_or_else(|| {
            StoreError::Inconsistent(format!("the log has no chain over its {records} records"))
        })?;
        Ok(Held {
            log: self.log.id(),
            records,
            chain,
        })
    }

    fn take_in(&mut self, sent: &Sent) -> Result<u64, StoreError> {
        // An answer to an older ask, or one that came twice: what it
        // holds is here already, or does not start where this log ends.
        if sent.after == self.log.records() {
            self.replicate(&sent.frames)?;
        }
        Ok(self.log.records())
    }

    fn lacks(&mut self, sent: &Sent) -> Result<Vec<Fingerprint>, StoreError> {
        if sent.after != self.log.records() {
            return Ok(Vec::new());
        }
        Ok(self.keys_lacked_for(&sent.frames)?.into_keys().collect())
    }

    fn take_keys(&mut self, sent: &Sent, keys: &[Key]) -> Result<(), StoreError> {
        if sent.after != self.log.records() {
            return Ok(());
        }
        self.keep_keys_for(&sent.frames, keys)
    }
}
