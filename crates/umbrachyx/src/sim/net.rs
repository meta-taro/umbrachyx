//! A network between a replica and its primary that a seed controls. An
//! answer on its way may be lost, arrive after a later one, arrive in
//! place of the one asked for, be cut short, or have a bit flipped.
//!
//! A real connection checks its bytes and keeps their order, so some of
//! this does not happen on one. A replica that is right whatever arrives
//! is right on any connection, and after one is made again.

use super::rng::Rng;
use crate::log::Storage;
use crate::store::follow::{Ask, Primary, Refusal, Sent, answer};
use crate::store::{Clock, Store};

/// Out of 100, per answer.
const LOST_PERCENT: u64 = 10;
const LATE_PERCENT: u64 = 15;
const CUT_PERCENT: u64 = 15;
const FLIP_PERCENT: u64 = 5;

/// What happened to answers so far.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Carried {
    pub lost: usize,
    /// An older answer arrived in place of the one asked for.
    pub late: usize,
    pub cut: usize,
    pub flipped: usize,
}

#[derive(Debug, Default)]
pub struct Net {
    /// An answer held back, to arrive in place of a later one.
    held_back: Option<Sent>,
    pub carried: Carried,
    /// Whether the last answer delivered had a bit flipped: the one case
    /// in which a replica may refuse what it was sent.
    pub last_was_flipped: bool,
}

/// A primary reached through a [`Net`].
pub struct Link<'a, S: Storage, C: Clock> {
    pub store: &'a mut Store<S, C>,
    pub net: &'a mut Net,
    pub rng: &'a mut Rng,
}

impl<S: Storage, C: Clock> Primary for Link<'_, S, C> {
    fn ask(&mut self, ask: &Ask) -> Result<Result<Sent, Refusal>, String> {
        let answer = answer(self.store, ask).map_err(|error| error.to_string())?;
        self.net.last_was_flipped = false;
        let sent = match answer {
            Ok(sent) => sent,
            // A refusal is small and arrives or does not.
            Err(refusal) => return Ok(Err(refusal)),
        };
        Ok(Ok(self.net.carry(sent, self.rng)?))
    }
}

impl Net {
    fn carry(&mut self, mut sent: Sent, rng: &mut Rng) -> Result<Sent, String> {
        if rng.percent(LOST_PERCENT) {
            self.carried.lost += 1;
            return Err("the answer was lost".to_owned());
        }
        if rng.percent(LATE_PERCENT) {
            // This one is held back; what was held back before, if
            // anything, arrives now.
            return match self.held_back.replace(sent) {
                Some(older) => {
                    self.carried.late += 1;
                    Ok(older)
                }
                None => {
                    self.carried.lost += 1;
                    Err("the answer is late".to_owned())
                }
            };
        }
        if !sent.frames.is_empty() && rng.percent(CUT_PERCENT) {
            let keep = rng.below(sent.frames.len() as u64) as usize;
            sent.frames.truncate(keep);
            self.carried.cut += 1;
        }
        if !sent.frames.is_empty() && rng.percent(FLIP_PERCENT) {
            let bit = rng.below(sent.frames.len() as u64 * 8) as usize;
            sent.frames[bit / 8] ^= 1 << (bit % 8);
            self.carried.flipped += 1;
            self.last_was_flipped = true;
        }
        Ok(sent)
    }
}
