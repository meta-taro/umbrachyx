//! A network between a replica and its primary that a seed controls. An
//! answer on its way may be lost, arrive after a later one, arrive in
//! place of the one asked for, be cut short, or have a bit flipped.
//!
//! The keys a replica asks for travel the same way, and meet the same.
//!
//! A real connection checks its bytes and keeps their order, so some of
//! this does not happen on one. A replica that is right whatever arrives
//! is right on any connection, and after one is made again.

use super::rng::Rng;
use crate::keys::Fingerprint;
use crate::log::Storage;
use crate::store::follow::{Ask, Feed, KeyAnswer, Primary, Refusal, Sent, answer};
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
    /// Answers that held keys, and those that arrived other than sent.
    pub keys: usize,
    pub keys_spoiled: usize,
    /// Asks for everything through an erasure.
    pub asked_through: usize,
}

#[derive(Debug, Default)]
pub struct Net {
    /// An answer held back, to arrive in place of a later one.
    held_back: Option<Sent>,
    /// Keys held back, to arrive in place of others asked for later.
    keys_held_back: Option<Vec<KeyAnswer>>,
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
        self.net.carried.asked_through += usize::from(ask.through.is_some());
        let answer = answer(self.store, ask).map_err(|error| error.to_string())?;
        self.net.last_was_flipped = false;
        let sent = match answer {
            Ok(sent) => sent,
            // A refusal is small and arrives or does not.
            Err(refusal) => return Ok(Err(refusal)),
        };
        Ok(Ok(self.net.carry(sent, self.rng)?))
    }

    fn keys(&mut self, wanted: &[Fingerprint]) -> Result<Option<Vec<KeyAnswer>>, String> {
        let answers = Feed::keys(self.store, wanted).map_err(|error| error.to_string())?;
        self.net.carry_keys(answers, self.rng).map(Some)
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

    fn carry_keys(
        &mut self,
        mut answers: Vec<KeyAnswer>,
        rng: &mut Rng,
    ) -> Result<Vec<KeyAnswer>, String> {
        self.carried.keys += 1;
        if rng.percent(LOST_PERCENT) {
            self.carried.keys_spoiled += 1;
            return Err("the keys were lost".to_owned());
        }
        if rng.percent(LATE_PERCENT) {
            self.carried.keys_spoiled += 1;
            return self
                .keys_held_back
                .replace(answers)
                .ok_or_else(|| "the keys are late".to_owned());
        }
        if rng.percent(CUT_PERCENT) {
            let keep = rng.below(answers.len() as u64) as usize;
            answers.truncate(keep);
            self.carried.keys_spoiled += 1;
        }
        if !answers.is_empty() && rng.percent(FLIP_PERCENT) {
            let at = rng.below(answers.len() as u64) as usize;
            match &mut answers[at] {
                KeyAnswer::Key(key) => {
                    let bit = rng.below(key.len() as u64 * 8) as usize;
                    key[bit / 8] ^= 1 << (bit % 8);
                }
                KeyAnswer::Erased { key: about, .. } | KeyAnswer::Unknown(about) => {
                    let bit = rng.below(about.len() as u64 * 8) as usize;
                    about[bit / 8] ^= 1 << (bit % 8);
                }
            }
            self.carried.keys_spoiled += 1;
        }
        Ok(answers)
    }
}
