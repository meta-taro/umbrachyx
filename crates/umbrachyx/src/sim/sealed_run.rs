//! One simulated run of a store whose values are encrypted: commits to the
//! keys of a few subjects on a few branches, erasures, failed writes to the
//! key file, interrupted syncs of an erasure, process crashes and power
//! cuts, all chosen by a seed. The keys themselves come from the operating
//! system's random source, so ciphertext differs between runs of one seed;
//! nothing the run decides or records depends on it.
//!
//! The promises, checked after every step and every restart:
//! - the store opens again, every time;
//! - every value an acknowledged commit wrote reads back, unless its
//!   subject was erased after the commit;
//! - once an erasure is acknowledged, or has reached the log, none of the
//!   subject's earlier values can be read again, on any branch or in any
//!   history, and they read as erased; values written after it read back;
//! - an erasure whose sync was interrupted is there whole or not at all.
//!
//! Commits whose log sync fails are the plain store's run's business.

use std::collections::BTreeMap;

use super::disk::{SimStorage, SyncFault};
use super::key_disk::{KeyFault, SimKeyStorage};
use super::rng::Rng;
use super::store_run::{SimClock, provenance};
use crate::keys::{KeyStorage, Keys};
use crate::log::{Edit, LogError};
use crate::store::{MAIN_BRANCH, Store, StoreError};

const STEPS: usize = 64;
/// Out of 100, per step; the rest cut power.
const COMMIT_PERCENT: u64 = 66;
const ERASE_PERCENT: u64 = 12;
const NEW_BRANCH_PERCENT: u64 = 4;
const PROCESS_CRASH_PERCENT: u64 = 9;
/// Out of 100, per commit or erasure.
const KEY_FAULT_PERCENT: u64 = 15;
/// Out of 100, per erasure that does not meet a key fault.
const LOG_FAULT_PERCENT: u64 = 10;
/// Few subjects and fields, so erasures keep meeting live values and
/// commits keep coming back to erased subjects.
const SUBJECTS: u64 = 6;
const FIELDS: u64 = 4;
const MAX_EDITS: u64 = 4;
/// Out of 100, per edit.
const REMOVE_PERCENT: u64 = 20;
/// An entry is a little over 70 bytes; tear anywhere in it.
const MAX_TORN: u64 = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Held {
    Value(Vec<u8>),
    Erased,
}

/// What each key should read as, on one branch. Keys never written, or
/// last removed, are absent.
type Model = BTreeMap<Vec<u8>, Held>;
type Models = BTreeMap<String, Model>;
type SealedStore = Store<SimStorage, SimClock>;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub trace: Vec<String>,
    pub commits: usize,
    pub erasures: usize,
    pub restarts: usize,
    /// Commits refused because a new key could not be written.
    pub key_faults_on_commit: usize,
    /// Erasures logged whose key file could not be rewritten at once.
    pub key_faults_on_erase: usize,
    /// Interrupted erasures that came back, and that did not.
    pub in_flight_kept: usize,
    pub in_flight_lost: usize,
    /// Commits to a subject erased before.
    pub writes_after_erasure: usize,
}

struct Run {
    rng: Rng,
    key_disk: SimKeyStorage,
    store: SealedStore,
    models: Models,
    /// Every value erased so far, by key, which must never read again.
    erased_values: Vec<(Vec<u8>, Vec<u8>)>,
    erased_subjects: Vec<Vec<u8>>,
    /// The subject of an erasure whose sync was interrupted.
    in_flight: Option<Vec<u8>>,
    outcome: Outcome,
}

pub fn run(seed: u64) -> Result<Outcome, String> {
    let mut rng = Rng::new(seed);
    let key_disk = SimKeyStorage::default();
    let clock = SimClock::new(&mut rng);
    let store = open(SimStorage::default(), &key_disk, clock)
        .map_err(|e| format!("empty store fails to open: {e}"))?;
    let mut run = Run {
        rng,
        key_disk,
        store,
        models: Models::from([(MAIN_BRANCH.to_owned(), Model::new())]),
        erased_values: Vec::new(),
        erased_subjects: Vec::new(),
        in_flight: None,
        outcome: Outcome::default(),
    };
    for step in 0..STEPS {
        run.step(step)?;
        run.check(step)?;
    }
    Ok(run.outcome)
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

impl Run {
    fn step(&mut self, step: usize) -> Result<(), String> {
        let roll = self.rng.below(100);
        if roll < COMMIT_PERCENT {
            self.commit(step)
        } else if roll < COMMIT_PERCENT + ERASE_PERCENT {
            self.erase(step)
        } else if roll < COMMIT_PERCENT + ERASE_PERCENT + NEW_BRANCH_PERCENT {
            self.new_branch(step)
        } else if roll < COMMIT_PERCENT + ERASE_PERCENT + NEW_BRANCH_PERCENT + PROCESS_CRASH_PERCENT
        {
            self.restart(step, false)
        } else {
            self.restart(step, true)
        }
    }

    fn commit(&mut self, step: usize) -> Result<(), String> {
        let branch = self.random_branch();
        let edits = self.random_edits();
        let faulted = self.maybe_key_fault();
        let result = self.store.commit(&branch, edits.clone(), provenance(step));
        let fault_hit = faulted && !self.key_disk.clear_fault();
        match result {
            Ok(_) => {
                let model = self.models.get_mut(&branch).expect("known branch");
                for edit in &edits {
                    match &edit.value {
                        Some(value) => model.insert(edit.key.clone(), Held::Value(value.clone())),
                        None => model.remove(&edit.key),
                    };
                }
                let revisits = edits
                    .iter()
                    .any(|edit| self.erased_subjects.contains(&subject_of(&edit.key)));
                self.outcome.writes_after_erasure += usize::from(revisits);
                self.outcome.commits += 1;
                self.trace(step, format!("commit {} edits to {branch}", edits.len()));
                Ok(())
            }
            Err(StoreError::Keys(_)) if fault_hit => {
                self.outcome.key_faults_on_commit += 1;
                self.trace(step, "commit refused: key file write failed".to_owned());
                Ok(())
            }
            Err(error) => Err(format!(
                "step {step}: commit to {branch} (key fault {fault_hit}) failed: {error}"
            )),
        }
    }

    fn erase(&mut self, step: usize) -> Result<(), String> {
        let subject = format!("p/{}", self.rng.below(SUBJECTS)).into_bytes();
        let key_fault = self.maybe_key_fault();
        let log_fault = (!key_fault && self.rng.percent(LOG_FAULT_PERCENT)).then(|| {
            if self.rng.percent(50) {
                SyncFault::KeepPending
            } else {
                SyncFault::DropPending
            }
        });
        if let Some(fault) = log_fault {
            self.store.storage_mut().fault_next_sync(fault);
        }
        let result = self.store.erase(&subject, provenance(step));
        let key_fault_hit = key_fault && !self.key_disk.clear_fault();
        let name = String::from_utf8_lossy(&subject).into_owned();
        match (result, log_fault) {
            (Ok(had_key), None) => {
                if !had_key && self.has_values(&subject) {
                    return Err(format!("step {step}: {name} has values but no key"));
                }
                self.forget(&subject);
                self.outcome.erasures += 1;
                self.trace(step, format!("erase {name}: {had_key}"));
                Ok(())
            }
            (Err(StoreError::Keys(_)), None) if key_fault_hit => {
                // Logged: it holds from here on, and the next open finishes it.
                self.forget(&subject);
                self.outcome.erasures += 1;
                self.outcome.key_faults_on_erase += 1;
                self.trace(step, format!("erase {name}: key file write failed"));
                Ok(())
            }
            (Err(StoreError::Log(LogError::Io(_))), Some(fault)) => {
                if fault == SyncFault::KeepPending {
                    self.in_flight = Some(subject);
                }
                self.trace(step, format!("erase {name}: sync fault {fault:?}"));
                let power = fault == SyncFault::DropPending || self.rng.percent(50);
                self.restart(step, power)
            }
            // The subject had no key, so nothing was written to fail.
            (Ok(false), Some(_)) => {
                self.store.storage_mut().clear_sync_fault();
                Ok(())
            }
            (result, fault) => Err(format!(
                "step {step}: erase {name} with log fault {fault:?}, key fault {key_fault_hit}, \
                 returned {result:?}"
            )),
        }
    }

    fn new_branch(&mut self, step: usize) -> Result<(), String> {
        let name = format!("b{}", self.models.len());
        let from = self.random_branch();
        self.store
            .create_branch(&name, &from)
            .map_err(|e| format!("step {step}: branch {name} from {from}: {e}"))?;
        let model = self.models[&from].clone();
        self.models.insert(name.clone(), model);
        self.trace(step, format!("branch {name} from {from}"));
        Ok(())
    }

    fn restart(&mut self, step: usize, power: bool) -> Result<(), String> {
        let disk = if power {
            self.store.storage_mut().power_cut(&mut self.rng)
        } else {
            self.store.storage_mut().process_crash()
        };
        self.outcome.restarts += 1;
        let clock = SimClock::new(&mut self.rng);
        self.store = open(disk, &self.key_disk, clock)
            .map_err(|e| format!("step {step}: store fails to reopen: {e}"))?;
        if let Some(subject) = self.in_flight.take() {
            if self.read_models() == self.models {
                self.outcome.in_flight_lost += 1;
            } else {
                self.forget(&subject);
                self.outcome.in_flight_kept += 1;
            }
        }
        self.trace(
            step,
            format!(
                "reopened after a {}",
                if power { "power cut" } else { "crash" }
            ),
        );
        Ok(())
    }

    /// Every promise, against what the store reads now.
    fn check(&self, step: usize) -> Result<(), String> {
        let held = self.read_models();
        if held != self.models {
            return Err(format!(
                "step {step}: the store reads {held:?}, expected {:?}",
                self.models
            ));
        }
        let erased_keys: Vec<Vec<u8>> = all_keys()
            .filter(|key| self.erased_subjects.contains(&subject_of(key)))
            .collect();
        for branch in self.models.keys() {
            for key in &erased_keys {
                let revealed = self
                    .store
                    .history(branch, key)
                    .into_iter()
                    .filter_map(|cell| cell.value);
                for value in revealed {
                    if self.erased_values.contains(&(key.clone(), value.to_vec())) {
                        return Err(format!(
                            "step {step}: an erased value of {} reads again on {branch}",
                            String::from_utf8_lossy(key)
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn read_models(&self) -> Models {
        self.models
            .keys()
            .map(|branch| {
                let model = all_keys()
                    .filter_map(|key| {
                        let cell = self.store.cell(branch, &key)?;
                        let held = match (cell.value, cell.erased) {
                            (Some(value), _) => Held::Value(value.to_vec()),
                            (None, true) => Held::Erased,
                            (None, false) => return None,
                        };
                        Some((key, held))
                    })
                    .collect();
                (branch.clone(), model)
            })
            .collect()
    }

    /// The subject's values on every branch become erased.
    fn forget(&mut self, subject: &[u8]) {
        for model in self.models.values_mut() {
            for (key, held) in model.iter_mut() {
                if subject_of(key) != subject {
                    continue;
                }
                if let Held::Value(value) = held {
                    self.erased_values.push((key.clone(), value.clone()));
                    *held = Held::Erased;
                }
            }
        }
        if !self.erased_subjects.iter().any(|s| s == subject) {
            self.erased_subjects.push(subject.to_vec());
        }
    }

    fn has_values(&self, subject: &[u8]) -> bool {
        self.models.values().any(|model| {
            model
                .iter()
                .any(|(key, held)| subject_of(key) == subject && matches!(held, Held::Value(_)))
        })
    }

    /// Sometimes makes the next write to the key file fail. Returns whether
    /// it did.
    fn maybe_key_fault(&mut self) -> bool {
        if !self.rng.percent(KEY_FAULT_PERCENT) {
            return false;
        }
        let fault = KeyFault {
            torn: self.rng.below(MAX_TORN) as usize,
            garbage: self.rng.percent(50).then(|| self.rng.next_u64()),
            new_survives: self.rng.percent(50),
        };
        self.key_disk.fail_next_write(fault);
        true
    }

    fn random_branch(&mut self) -> String {
        let index = self.rng.below(self.models.len() as u64) as usize;
        self.models.keys().nth(index).expect("in range").clone()
    }

    fn random_edits(&mut self) -> Vec<Edit> {
        let count = 1 + self.rng.below(MAX_EDITS);
        (0..count)
            .map(|_| {
                let key = format!("p/{}/f{}", self.rng.below(SUBJECTS), self.rng.below(FIELDS))
                    .into_bytes();
                let value = (!self.rng.percent(REMOVE_PERCENT))
                    .then(|| self.rng.next_u64().to_le_bytes().to_vec());
                Edit { key, value }
            })
            .collect()
    }

    fn trace(&mut self, step: usize, event: String) {
        self.outcome.trace.push(format!("{step}: {event}"));
    }
}

fn all_keys() -> impl Iterator<Item = Vec<u8>> {
    (0..SUBJECTS).flat_map(|s| (0..FIELDS).map(move |f| format!("p/{s}/f{f}").into_bytes()))
}

/// `p/3/f1` belongs to `p/3`.
fn subject_of(key: &[u8]) -> Vec<u8> {
    let second_slash = key
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .nth(1)
        .map_or(key.len(), |(at, _)| at);
    key[..second_slash].to_vec()
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
                     --lib sim::sealed_run::tests::replay_seed -- --ignored --exact --nocapture"
                )
            });
            total.commits += outcome.commits;
            total.erasures += outcome.erasures;
            total.restarts += outcome.restarts;
            total.key_faults_on_commit += outcome.key_faults_on_commit;
            total.key_faults_on_erase += outcome.key_faults_on_erase;
            total.in_flight_kept += outcome.in_flight_kept;
            total.in_flight_lost += outcome.in_flight_lost;
            total.writes_after_erasure += outcome.writes_after_erasure;
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

        assert!(total.restarts >= 1_000, "{total:?}");
        assert!(total.erasures >= 1_000, "{total:?}");
        assert!(total.key_faults_on_commit >= 100, "{total:?}");
        assert!(total.key_faults_on_erase >= 100, "{total:?}");
        assert!(total.in_flight_kept >= 20, "{total:?}");
        assert!(total.in_flight_lost >= 20, "{total:?}");
        assert!(total.writes_after_erasure >= 1_000, "{total:?}");
    }

    #[test]
    #[ignore = "10,000 seeds; run with --ignored"]
    fn ten_thousand_seeds_sealed() {
        let seeds = std::env::var(SEEDS_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(FULL_SEEDS);
        let total = run_seeds(seeds);
        println!(
            "sealed store, {seeds} seeds: {} commits, {} erasures, {} restarts, \
             {} commits refused for a failed key write, {} erasures whose key file \
             was finished later, interrupted erasures kept {} and lost {}, \
             {} commits to erased subjects",
            total.commits,
            total.erasures,
            total.restarts,
            total.key_faults_on_commit,
            total.key_faults_on_erase,
            total.in_flight_kept,
            total.in_flight_lost,
            total.writes_after_erasure
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
