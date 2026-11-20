//! One simulated run of a replica whose keys travel: a store whose values
//! are encrypted is written to and has subjects erased, and a second
//! store follows it over a network that loses, delays, cuts short and
//! damages both the records and the keys it carries. The replica crashes
//! and loses power, and writes to its key file fail. All chosen by a
//! seed; the keys themselves come from the operating system's random
//! source, and nothing the run decides depends on them.
//!
//! The promises, checked after every step:
//! - the replica opens again, every time: no record is on its disk
//!   before the key it is sealed under;
//! - it never holds a record its primary does not;
//! - it keeps no key but the one the records name, whatever arrives;
//! - it stops for good only when its primary destroyed a key before the
//!   replica had it, and then keeps nothing of what needed the key;
//! - once it has every record, it reads every value its primary reads,
//!   and none of an erased subject.

use super::disk::SimStorage;
use super::key_disk::{KeyFault, SimKeyStorage};
use super::net::{Carried, Link, Net};
use super::rng::Rng;
use super::store_run::{SimClock, provenance};
use crate::keys::{KeyStorage, Keys};
use crate::log::Edit;
use crate::store::follow::{FollowError, WithKeys, follow_once};
use crate::store::{MAIN_BRANCH, Store, StoreError};

const STEPS: usize = 64;
/// Out of 100, per step; the rest are commits.
const ERASE_PERCENT: u64 = 8;
const NEW_BRANCH_PERCENT: u64 = 4;
/// Out of 100, after a step.
const PULL_PERCENT: u64 = 60;
const RESTART_PERCENT: u64 = 10;
/// Out of 100, per pull.
const KEY_FAULT_PERCENT: u64 = 10;
/// Many subjects, so the replica keeps meeting one it has no key for;
/// few enough that erased ones are written again.
const SUBJECTS: u64 = 12;
const FIELDS: u64 = 3;
const MAX_EDITS: u64 = 3;
const MAX_PULL_BYTES: u64 = 2_048;
const MAX_TORN: u64 = 80;
const REPLICA_SALT: u64 = 0x6b65_7973;

type SealedStore = Store<SimStorage, SimClock>;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub trace: Vec<String>,
    pub commits: usize,
    pub erasures: usize,
    /// Times the replica took records in.
    pub pulls: usize,
    pub restarts: usize,
    /// Pulls that failed on the replica's key file.
    pub key_faults: usize,
    /// Runs in which the replica stopped for a key destroyed before it
    /// had it, and those of them in which it was still stopped when
    /// asked again at the end, with answers as large as it would take.
    pub stopped: usize,
    pub stopped_at_end: usize,
    /// Runs in which it read everything its primary reads at the end.
    pub same: usize,
    pub carried: Carried,
}

struct Run {
    rng: Rng,
    store: SealedStore,
    branches: Vec<String>,
    replica: SealedStore,
    replica_keys: SimKeyStorage,
    following: Rng,
    net: Net,
    is_stopped: bool,
    outcome: Outcome,
}

fn open(
    disk: SimStorage,
    key_disk: &SimKeyStorage,
    clock: SimClock,
) -> Result<SealedStore, String> {
    let storage: Box<dyn KeyStorage + Send> = Box::new(key_disk.clone());
    let keys = Keys::open(storage).map_err(|e| e.to_string())?;
    Store::open_sealed(disk, keys, clock).map_err(|e| e.to_string())
}

pub fn run(seed: u64) -> Result<Outcome, String> {
    let mut rng = Rng::new(seed);
    let mut following = Rng::new(seed ^ REPLICA_SALT);
    let store = open(
        SimStorage::default(),
        &SimKeyStorage::default(),
        SimClock::new(&mut rng),
    )
    .map_err(|e| format!("empty store fails to open: {e}"))?;
    let replica_keys = SimKeyStorage::default();
    let replica = open(
        SimStorage::default(),
        &replica_keys,
        SimClock::new(&mut following),
    )
    .map_err(|e| format!("empty replica fails to open: {e}"))?;
    let mut run = Run {
        rng,
        store,
        branches: vec![MAIN_BRANCH.to_owned()],
        replica,
        replica_keys,
        following,
        net: Net::default(),
        is_stopped: false,
        outcome: Outcome::default(),
    };
    for step in 0..STEPS {
        run.step(step)?;
        run.follow(step)?;
        if run.replica.log_records() > run.store.log_records() {
            return Err(format!("step {step}: the replica holds more records"));
        }
    }
    run.finish()?;
    Ok(run.outcome)
}

impl Run {
    fn step(&mut self, step: usize) -> Result<(), String> {
        let roll = self.rng.below(100);
        if roll < ERASE_PERCENT {
            let subject = format!("p/{}", self.rng.below(SUBJECTS)).into_bytes();
            self.store
                .erase(&subject, provenance(step))
                .map_err(|e| format!("step {step}: erase: {e}"))?;
            self.outcome.erasures += 1;
            self.trace(step, "erase".to_owned());
        } else if roll < ERASE_PERCENT + NEW_BRANCH_PERCENT {
            let name = format!("b{}", self.branches.len());
            let from = self.random_branch();
            self.store
                .create_branch(&name, &from)
                .map_err(|e| format!("step {step}: branch: {e}"))?;
            self.branches.push(name);
        } else {
            let branch = self.random_branch();
            let edits = self.random_edits();
            self.store
                .commit(&branch, edits, provenance(step))
                .map_err(|e| format!("step {step}: commit: {e}"))?;
            self.outcome.commits += 1;
        }
        Ok(())
    }

    /// What the replica does after a step: sometimes it restarts,
    /// sometimes it asks for more.
    fn follow(&mut self, step: usize) -> Result<(), String> {
        if self.following.percent(RESTART_PERCENT) {
            let disk = if self.following.percent(50) {
                self.replica.storage_mut().power_cut(&mut self.following)
            } else {
                self.replica.storage_mut().process_crash()
            };
            let clock = SimClock::new(&mut self.following);
            self.replica = open(disk, &self.replica_keys, clock)
                .map_err(|e| format!("step {step}: replica fails to reopen: {e}"))?;
            self.outcome.restarts += 1;
            self.trace(step, "replica reopened".to_owned());
        }
        if !self.is_stopped && self.following.percent(PULL_PERCENT) {
            self.pull(step)?;
        }
        Ok(())
    }

    fn pull(&mut self, step: usize) -> Result<(), String> {
        let max_bytes = 1 + self.following.below(MAX_PULL_BYTES) as usize;
        let before = self.replica.log_records();
        let faulted = self.following.percent(KEY_FAULT_PERCENT);
        if faulted {
            self.replica_keys.fail_next_write(KeyFault {
                torn: self.following.below(MAX_TORN) as usize,
                garbage: self
                    .following
                    .percent(50)
                    .then(|| self.following.next_u64()),
                new_survives: self.following.percent(50),
            });
        }
        let mut link = Link {
            store: &mut self.store,
            net: &mut self.net,
            rng: &mut self.following,
        };
        let followed = follow_once(&mut link, &mut self.replica, max_bytes);
        let fault_hit = faulted && !self.replica_keys.clear_fault();
        let kept_nothing = self.replica.log_records() == before;
        match followed {
            Ok(progress) => {
                self.outcome.pulls += usize::from(progress.held > before);
                self.trace(step, format!("replica holds {}", progress.held));
            }
            Err(FollowError::Link(_)) if kept_nothing => {}
            // Keeping a key failed and nothing was taken in; or the
            // records are in, and an erasure among them is finished when
            // the replica next opens.
            Err(FollowError::Store(StoreError::Keys(_))) if fault_hit => {
                self.outcome.key_faults += 1;
                self.trace(step, "replica's key file write failed".to_owned());
            }
            Err(FollowError::Store(StoreError::KeysErased(_))) if kept_nothing => {
                self.stop(step)?;
            }
            // Bytes that do not check out are refused whole.
            Err(FollowError::Store(StoreError::Log(_)))
                if self.net.last_was_flipped && kept_nothing => {}
            Err(error) => {
                return Err(format!(
                    "step {step}: the replica, with {before} records and now {}, cannot follow \
                     (key fault {fault_hit}): {error}",
                    self.replica.log_records()
                ));
            }
        }
        Ok(())
    }

    /// The replica may stop for a destroyed key only if the primary did
    /// destroy one that the records after the replica's need. Then, sent
    /// those records one at a time with nothing in the way, it stops
    /// again; if it gets to the end, nothing was destroyed and whatever
    /// the network delivered made it stop.
    fn stop(&mut self, step: usize) -> Result<(), String> {
        loop {
            match follow_once(&mut WithKeys(&mut self.store), &mut self.replica, 1) {
                Ok(progress) if progress.is_caught_up() => {
                    return Err(format!(
                        "step {step}: the replica stopped for a destroyed key, and none it \
                         needs was destroyed"
                    ));
                }
                Ok(_) => {}
                Err(FollowError::Store(StoreError::KeysErased(_))) => break,
                Err(error) => return Err(format!("step {step}: stopped, then: {error}")),
            }
        }
        self.is_stopped = true;
        self.outcome.stopped = 1;
        self.trace(
            step,
            "replica stopped: a key was destroyed first".to_owned(),
        );
        Ok(())
    }

    /// The network mends: every answer from here on arrives as sent, and
    /// the replica's key file takes its writes.
    fn finish(&mut self) -> Result<(), String> {
        self.outcome.carried = self.net.carried;
        // A replica that stopped did so on an answer of a size the run
        // chose, often one record. Sent everything at once, the erasure
        // may come with the value, and then it goes on.
        loop {
            match follow_once(
                &mut WithKeys(&mut self.store),
                &mut self.replica,
                usize::MAX,
            ) {
                Ok(progress) if progress.is_caught_up() => break,
                Ok(_) => {}
                Err(FollowError::Store(StoreError::KeysErased(_))) => {
                    if !self.is_stopped {
                        self.stop(STEPS)?;
                    }
                    self.outcome.stopped_at_end = 1;
                    return Ok(());
                }
                Err(error) => return Err(format!("the replica cannot follow to the end: {error}")),
            }
        }
        for branch in &self.branches {
            if self.replica.entries(branch) != self.store.entries(branch) {
                return Err(format!("the replica reads other values on {branch}"));
            }
        }
        if self.replica.erasures() != self.store.erasures() {
            return Err("the replica has other erasures".to_owned());
        }
        // And from its disk alone, as after a restart.
        let disk = self.replica.storage_mut().process_crash();
        let clock = SimClock::new(&mut self.following);
        let reopened = open(disk, &self.replica_keys, clock)
            .map_err(|e| format!("the replica fails to reopen at the end: {e}"))?;
        for branch in &self.branches {
            if reopened.entries(branch) != self.store.entries(branch) {
                return Err(format!(
                    "reopened, the replica reads other values on {branch}"
                ));
            }
        }
        self.outcome.same = 1;
        Ok(())
    }

    fn random_branch(&mut self) -> String {
        let index = self.rng.below(self.branches.len() as u64) as usize;
        self.branches[index].clone()
    }

    fn random_edits(&mut self) -> Vec<Edit> {
        let count = 1 + self.rng.below(MAX_EDITS);
        (0..count)
            .map(|_| Edit {
                key: format!("p/{}/f{}", self.rng.below(SUBJECTS), self.rng.below(FIELDS))
                    .into_bytes(),
                value: Some(self.rng.next_u64().to_le_bytes().to_vec()),
            })
            .collect()
    }

    fn trace(&mut self, step: usize, event: String) {
        self.outcome.trace.push(format!("{step}: {event}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEEDS_ENV: &str = "UMBRACHYX_SEEDS";
    const QUICK_SEEDS: u64 = 300;
    const FULL_SEEDS: u64 = 10_000;

    fn run_seeds(seeds: u64) -> Outcome {
        let mut total = Outcome::default();
        for seed in 0..seeds {
            let outcome = run(seed).unwrap_or_else(|error| {
                panic!(
                    "seed {seed}: {error}\nreplay: UMBRACHYX_SEED={seed} cargo test -p umbrachyx \
                     --lib sim::keys_run::tests::replay_seed -- --ignored --exact --nocapture"
                )
            });
            total.commits += outcome.commits;
            total.erasures += outcome.erasures;
            total.pulls += outcome.pulls;
            total.restarts += outcome.restarts;
            total.key_faults += outcome.key_faults;
            total.stopped += outcome.stopped;
            total.stopped_at_end += outcome.stopped_at_end;
            total.same += outcome.same;
            total.carried.keys += outcome.carried.keys;
            total.carried.keys_spoiled += outcome.carried.keys_spoiled;
            total.carried.lost += outcome.carried.lost;
            total.carried.late += outcome.carried.late;
            total.carried.cut += outcome.carried.cut;
            total.carried.flipped += outcome.carried.flipped;
        }
        total
    }

    #[test]
    fn same_seed_runs_the_same_way() {
        assert_eq!(run(42), run(42));
    }

    /// The promises are only tested if the faults happen.
    #[test]
    fn faults_actually_happen() {
        let total = run_seeds(QUICK_SEEDS);

        assert!(total.pulls >= 3_000, "{total:?}");
        assert!(total.restarts >= 1_000, "{total:?}");
        assert!(total.carried.keys >= 2_000, "{total:?}");
        assert!(total.carried.keys_spoiled >= 500, "{total:?}");
        assert!(total.key_faults >= 50, "{total:?}");
        assert!(total.same >= 100, "{total:?}");
        assert!(total.stopped >= 10, "{total:?}");
        assert!(total.stopped_at_end >= 5, "{total:?}");
    }

    #[test]
    #[ignore = "10,000 seeds; run with --ignored"]
    fn ten_thousand_seeds_keys() {
        let seeds = std::env::var(SEEDS_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(FULL_SEEDS);
        let total = run_seeds(seeds);
        println!(
            "keys that travel, {seeds} seeds: {} commits, {} erasures; a replica took records \
             in {} times and restarted {} times; keys were sent {} times, {} of them lost, \
             late, cut short or damaged; {} writes to its key file failed; it read what its \
             primary reads at the end of {} runs; it stopped for a key destroyed first in {}, \
             and was still stopped in {} when sent everything at once",
            total.commits,
            total.erasures,
            total.pulls,
            total.restarts,
            total.carried.keys,
            total.carried.keys_spoiled,
            total.key_faults,
            total.same,
            total.stopped,
            total.stopped_at_end
        );
    }

    #[test]
    #[ignore = "replays UMBRACHYX_SEED"]
    fn replay_seed() {
        let Some(seed) = std::env::var("UMBRACHYX_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
        else {
            return;
        };
        match run(seed) {
            Ok(outcome) => println!("{}", outcome.trace.join("\n")),
            Err(error) => panic!("seed {seed}: {error}"),
        }
    }
}
