//! One simulated run of the store: commits of several edits to several
//! branches, new branches, failed syncs, process crashes and power cuts, and
//! a clock that sometimes goes backwards, all chosen by a seed.
//!
//! The promises, checked after every restart:
//! - every branch exists and holds exactly what the acknowledged commits
//!   wrote to it, and branches see nothing of each other's later commits;
//! - a commit whose sync was interrupted is there whole or not at all, and
//!   so is a group of commits written with one sync;
//! - recorded times never go backwards, across commits and restarts.
//!
//! Damage to the disk is the log's business and is covered by its own run.

use std::collections::BTreeMap;

use super::disk::{SimStorage, SyncFault};
use super::rng::Rng;
use crate::log::{Edit, LogError};
use crate::store::{
    Clock, CommitRequest, MAIN_BRANCH, Merged, Provenance, Reverted, Store, StoreError,
};

const STEPS: usize = 64;
/// Out of 100, per step; the rest cut power.
const COMMIT_PERCENT: u64 = 80;
const PROCESS_CRASH_PERCENT: u64 = 10;
/// Out of 100, per commit.
const SYNC_FAULT_PERCENT: u64 = 6;
/// Small, so commits keep overwriting and removing each other's keys.
const KEY_SPACE: u64 = 200;
const MAX_EDITS: u64 = 6;
/// Out of 100, per edit.
const REMOVE_PERCENT: u64 = 20;
/// Out of 100, per commit step: make a branch, merge two branches, or undo
/// a branch's latest commit instead.
const NEW_BRANCH_PERCENT: u64 = 8;
const MERGE_PERCENT: u64 = 8;
const REVERT_PERCENT: u64 = 4;
/// Out of 100, per plain commit step: write several commits with one sync.
const GROUP_PERCENT: u64 = 25;
const MAX_GROUP: u64 = 5;
/// Out of 100, per reading of the clock.
const CLOCK_BACK_PERCENT: u64 = 5;
const START_MICROS: u64 = 1_800_000_000_000_000;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
/// What each branch should hold.
type Models = BTreeMap<String, Model>;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub trace: Vec<String>,
    pub commits: usize,
    /// Groups of commits written with one sync, also counted in `commits`.
    pub groups: usize,
    pub restarts: usize,
    /// Interrupted commits that came back whole, and that did not.
    pub in_flight_kept: usize,
    pub in_flight_lost: usize,
    pub clock_went_back: usize,
    pub branches_made: usize,
    pub merges: usize,
    pub merge_conflicts: usize,
    pub reverts: usize,
}

/// A clock a seed controls. It mostly moves forward a little, and sometimes
/// jumps back, as a wall clock corrected by NTP can.
pub struct SimClock {
    rng: Rng,
    now: u64,
    went_back: usize,
}

impl SimClock {
    fn new(rng: &mut Rng) -> Self {
        Self {
            rng: Rng::new(rng.next_u64()),
            now: START_MICROS,
            went_back: 0,
        }
    }
}

impl Clock for SimClock {
    fn now_micros(&mut self) -> u64 {
        if self.rng.percent(CLOCK_BACK_PERCENT) {
            self.now = self.now.saturating_sub(self.rng.below(1_000_000));
            self.went_back += 1;
        } else {
            self.now += self.rng.below(2_000);
        }
        self.now
    }
}

struct Run {
    rng: Rng,
    store: Store<SimStorage, SimClock>,
    models: Models,
    /// The branch and edits of each commit whose sync was interrupted, in
    /// order: one commit, or a group.
    in_flight: Option<Vec<(String, Vec<Edit>)>>,
    /// The time of the latest acknowledged commit.
    acknowledged_time: u64,
    outcome: Outcome,
}

pub fn run(seed: u64) -> Result<Outcome, String> {
    let mut rng = Rng::new(seed);
    let clock = SimClock::new(&mut rng);
    let store = Store::open(SimStorage::default(), clock)
        .map_err(|e| format!("empty store fails to open: {e}"))?;
    let mut run = Run {
        rng,
        store,
        models: Models::from([(MAIN_BRANCH.to_owned(), Model::new())]),
        in_flight: None,
        acknowledged_time: 0,
        outcome: Outcome::default(),
    };
    for step in 0..STEPS {
        run.step(step)?;
    }
    run.outcome.clock_went_back += run.store.clock_mut().went_back;
    Ok(run.outcome)
}

impl Run {
    fn step(&mut self, step: usize) -> Result<(), String> {
        let roll = self.rng.below(100);
        if roll < COMMIT_PERCENT {
            self.commit(step)
        } else if roll < COMMIT_PERCENT + PROCESS_CRASH_PERCENT {
            self.restart(step, false)
        } else {
            self.restart(step, true)
        }
    }

    fn commit(&mut self, step: usize) -> Result<(), String> {
        if self.rng.percent(NEW_BRANCH_PERCENT) {
            return self.new_branch(step);
        }
        if self.rng.percent(MERGE_PERCENT) {
            return self.merge(step);
        }
        if self.rng.percent(REVERT_PERCENT) {
            return self.revert_latest(step);
        }
        let count = if self.rng.percent(GROUP_PERCENT) {
            2 + self.rng.below(MAX_GROUP - 1)
        } else {
            1
        };
        let commits: Vec<(String, Vec<Edit>)> = (0..count)
            .map(|_| (self.random_branch(), self.random_edits()))
            .collect();
        let fault = if self.rng.percent(SYNC_FAULT_PERCENT) {
            Some(if self.rng.percent(50) {
                SyncFault::KeepPending
            } else {
                SyncFault::DropPending
            })
        } else {
            None
        };
        if let Some(fault) = fault {
            self.store.storage_mut().fault_next_sync(fault);
        }
        match (self.write(step, &commits), fault) {
            (Ok(()), None) => {
                for (branch, edits) in &commits {
                    apply(self.models.get_mut(branch).expect("known branch"), edits);
                    self.trace(step, format!("commit {} edits to {branch}", edits.len()));
                }
                self.outcome.commits += commits.len();
                self.outcome.groups += usize::from(commits.len() > 1);
                self.check_time(step)?;
                self.acknowledged_time = self.store.last_time();
                Ok(())
            }
            (Err(StoreError::Log(LogError::Io(_))), Some(fault)) => {
                if fault == SyncFault::KeepPending {
                    self.in_flight = Some(commits);
                }
                self.trace(step, format!("sync fault {fault:?}"));
                let power = fault == SyncFault::DropPending || self.rng.percent(50);
                self.restart(step, power)
            }
            (result, fault) => Err(format!(
                "step {step}: commit with fault {fault:?} returned {result:?}"
            )),
        }
    }

    /// One commit through `commit`, several through `commit_all`.
    fn write(&mut self, step: usize, commits: &[(String, Vec<Edit>)]) -> Result<(), StoreError> {
        if let [(branch, edits)] = commits {
            return self
                .store
                .commit(branch, edits.clone(), provenance(step))
                .map(drop);
        }
        let requests = commits
            .iter()
            .map(|(branch, edits)| CommitRequest {
                branch: branch.clone(),
                edits: edits.clone(),
                provenance: provenance(step),
            })
            .collect();
        for result in self.store.commit_all(requests)? {
            result?;
        }
        Ok(())
    }

    fn new_branch(&mut self, step: usize) -> Result<(), String> {
        let name = format!("b{}", self.models.len());
        let from = self.random_branch();
        self.store
            .create_branch(&name, &from)
            .map_err(|e| format!("step {step}: branch {name} from {from}: {e}"))?;
        let model = self.models[&from].clone();
        self.models.insert(name.clone(), model);
        self.outcome.branches_made += 1;
        self.trace(step, format!("branch {name} from {from}"));
        Ok(())
    }

    /// Merge semantics are tested on their own; here the point is that what
    /// a merge wrote comes back after a restart. So the store's own result
    /// becomes what the branch should hold.
    fn merge(&mut self, step: usize) -> Result<(), String> {
        let into = self.random_branch();
        let from = self.random_branch();
        let merged = self
            .store
            .merge(&into, &from, provenance(step))
            .map_err(|e| format!("step {step}: merge {from} into {into}: {e}"))?;
        match merged {
            Merged::Commit(_) => self.outcome.merges += 1,
            Merged::Conflicts(_) => self.outcome.merge_conflicts += 1,
            Merged::UpToDate => {}
        }
        self.models
            .insert(into.clone(), entries_of(&self.store, &into));
        self.trace(step, format!("merge {from} into {into}: {merged:?}"));
        Ok(())
    }

    /// Undoing a branch's latest commit cannot conflict, and must give back
    /// what the branch held before it.
    fn revert_latest(&mut self, step: usize) -> Result<(), String> {
        let branch = self.random_branch();
        let Some(head) = self.store.head(&branch) else {
            return Ok(());
        };
        let before = self
            .store
            .commit_info(&head)
            .and_then(|info| info.parent)
            .map(|parent| self.store.entries_at(&parent))
            .unwrap_or_default();
        let reverted = self
            .store
            .revert(&branch, &head, provenance(step))
            .map_err(|e| format!("step {step}: revert on {branch}: {e}"))?;
        if !matches!(reverted, Reverted::Commit(_)) {
            return Err(format!(
                "step {step}: undoing the latest commit on {branch} gave {reverted:?}"
            ));
        }
        let held = entries_of(&self.store, &branch);
        let expected: Model = before
            .into_iter()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
            .collect();
        if held != expected {
            return Err(format!(
                "step {step}: undoing the latest commit on {branch} did not restore it"
            ));
        }
        self.models.insert(branch.clone(), held);
        self.outcome.reverts += 1;
        self.trace(step, format!("revert latest on {branch}"));
        Ok(())
    }

    fn random_branch(&mut self) -> String {
        let index = self.rng.below(self.models.len() as u64) as usize;
        self.models.keys().nth(index).expect("in range").clone()
    }

    fn restart(&mut self, step: usize, power: bool) -> Result<(), String> {
        let disk = if power {
            self.store.storage_mut().power_cut(&mut self.rng)
        } else {
            self.store.storage_mut().process_crash()
        };
        self.outcome.restarts += 1;
        // The clock is replaced on restart; count what the old one did.
        self.outcome.clock_went_back += self.store.clock_mut().went_back;
        let clock = SimClock::new(&mut self.rng);
        self.store = Store::open(disk, clock)
            .map_err(|e| format!("step {step}: store fails to reopen: {e}"))?;
        self.check_recovered(step)?;
        self.check_time(step)?;
        self.acknowledged_time = self.store.last_time();
        Ok(())
    }

    fn check_recovered(&mut self, step: usize) -> Result<(), String> {
        let branches: Vec<&str> = self.store.branches().collect();
        if !branches
            .iter()
            .copied()
            .eq(self.models.keys().map(String::as_str))
        {
            return Err(format!(
                "step {step}: branches {branches:?} came back, expected {:?}",
                self.models.keys().collect::<Vec<_>>()
            ));
        }
        let held: Models = self
            .models
            .keys()
            .map(|branch| (branch.clone(), entries_of(&self.store, branch)))
            .collect();
        if held == self.models {
            if self.in_flight.take().is_some() {
                self.outcome.in_flight_lost += 1;
            }
        } else {
            let Some(in_flight) = self.in_flight.take() else {
                return Err(format!(
                    "step {step}: branches do not hold what acknowledged commits wrote"
                ));
            };
            let mut with_in_flight = self.models.clone();
            for (branch, edits) in &in_flight {
                apply(with_in_flight.get_mut(branch).expect("known branch"), edits);
            }
            if held != with_in_flight {
                return Err(format!(
                    "step {step}: branches hold neither the acknowledged state nor that \
                     state with every interrupted commit"
                ));
            }
            self.models = with_in_flight;
            self.outcome.in_flight_kept += 1;
        }
        let head = self.store.head(MAIN_BRANCH).map(|id| hex(&id));
        self.trace(step, format!("reopened, main at {head:?}"));
        Ok(())
    }

    fn check_time(&self, step: usize) -> Result<(), String> {
        if self.store.last_time() < self.acknowledged_time {
            return Err(format!(
                "step {step}: recorded time went back from {} to {}",
                self.acknowledged_time,
                self.store.last_time()
            ));
        }
        Ok(())
    }

    fn random_edits(&mut self) -> Vec<Edit> {
        let count = 1 + self.rng.below(MAX_EDITS);
        (0..count)
            .map(|_| {
                let key = format!("k{:03}", self.rng.below(KEY_SPACE)).into_bytes();
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

fn provenance(step: usize) -> Provenance {
    Provenance {
        who: "agent:sim".to_owned(),
        why: format!("step {step}"),
        approved_by: None,
    }
}

fn apply(model: &mut Model, edits: &[Edit]) {
    for edit in edits {
        match &edit.value {
            Some(value) => model.insert(edit.key.clone(), value.clone()),
            None => model.remove(&edit.key),
        };
    }
}

fn entries_of(store: &Store<SimStorage, SimClock>, branch: &str) -> Model {
    store
        .entries(branch)
        .into_iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
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
                     --lib sim::store_run::tests::replay_seed -- --ignored --exact --nocapture"
                )
            });
            total.commits += outcome.commits;
            total.groups += outcome.groups;
            total.restarts += outcome.restarts;
            total.in_flight_kept += outcome.in_flight_kept;
            total.in_flight_lost += outcome.in_flight_lost;
            total.clock_went_back += outcome.clock_went_back;
            total.branches_made += outcome.branches_made;
            total.merges += outcome.merges;
            total.merge_conflicts += outcome.merge_conflicts;
            total.reverts += outcome.reverts;
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
        assert!(total.groups >= 500, "{total:?}");
        assert!(total.in_flight_kept >= 20, "{total:?}");
        assert!(total.in_flight_lost >= 20, "{total:?}");
        assert!(total.clock_went_back >= 100, "{total:?}");
        assert!(total.branches_made >= 500, "{total:?}");
        assert!(total.merges >= 100, "{total:?}");
        assert!(total.merge_conflicts >= 50, "{total:?}");
        assert!(total.reverts >= 100, "{total:?}");
    }

    #[test]
    #[ignore = "10,000 seeds; run with --ignored"]
    fn ten_thousand_seeds() {
        let seeds = std::env::var(SEEDS_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(FULL_SEEDS);
        let total = run_seeds(seeds);
        println!(
            "store, {seeds} seeds: {} commits ({} groups), {} restarts, interrupted commits kept {} \
             and lost {}, clock went back {} times, {} branches made, {} merges, \
             {} merges refused for conflicts, {} reverts",
            total.commits,
            total.groups,
            total.restarts,
            total.in_flight_kept,
            total.in_flight_lost,
            total.clock_went_back,
            total.branches_made,
            total.merges,
            total.merge_conflicts,
            total.reverts
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
