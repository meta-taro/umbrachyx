//! One simulated run of the tables that keep no history: puts answered
//! before they are on disk, puts and commits that wait for it, records of
//! other kinds, failed syncs, process crashes and power cuts, all chosen
//! by a seed.
//!
//! Three tables: `cache` answers a put before it is on disk when the put
//! comes the way a committer sends it, `session` keeps no history and
//! waits for the disk, `player` keeps its history.
//!
//! The promises, checked at every step and after every restart:
//! - a read sees every write answered so far, on disk or not;
//! - whatever happens, the log opens again;
//! - what comes back is everything answered up to some moment and nothing
//!   answered after it: the writes lost are always the last ones;
//! - that moment is never before the last write that waited for the disk:
//!   a commit, a put to `session`, a put made on the store itself, a new
//!   branch, or a call that writes the held puts. So only puts to `cache`
//!   answered since then can be lost, and a write that waited never comes
//!   back without the puts answered before it;
//! - a write whose sync was interrupted is there whole, with every put
//!   answered before it, or not at all;
//! - once the log has refused a write, no put is answered at all, so
//!   memory does not run further ahead of the disk;
//! - a replica is never handed a put that is not on disk, and once it has
//!   every record it holds what the store holds.

use std::collections::BTreeMap;

use super::disk::{SimStorage, SyncFault};
use super::key_disk::{KeyFault, SimKeyStorage};
use super::rng::Rng;
use super::store_run::{SimClock, provenance};
use crate::keys::{KeyStorage, Keys};
use crate::log::{Edit, Engine, LogError};
use crate::store::{CommitRequest, MAIN_BRANCH, Request, Store, StoreError};

const STEPS: usize = 96;
/// Out of 100, per step. The rest cut power.
const EARLY_PERCENT: u64 = 45;
const WAITING_PERCENT: u64 = 25;
const DIRECT_PERCENT: u64 = 5;
const SYNC_PUTS_PERCENT: u64 = 5;
const BRANCH_PERCENT: u64 = 4;
const PROCESS_CRASH_PERCENT: u64 = 8;
/// Out of 100, per write that syncs.
const SYNC_FAULT_PERCENT: u64 = 8;
/// Small, so writes keep overwriting and removing each other's keys.
const KEY_SPACE: u64 = 40;
/// How often puts answered early are held by the thread that read
/// them, in a hundred.
const HELD_BY_THE_READER_PERCENT: u64 = 50;
/// Out of 100, per write that waits for the disk, when values are
/// sealed and its sync was not made to fail.
const KEY_FAULT_PERCENT: u64 = 10;
/// An entry of the key file is a little over 70 bytes; tear anywhere in
/// it.
const MAX_TORN: u64 = 80;
const MAX_REQUESTS: u64 = 4;
const MAX_EDITS: u64 = 4;
const REMOVE_PERCENT: u64 = 20;
/// How often, after a step, the replica asks for more, and the most
/// bytes it asks for.
const REPLICA_PULL_PERCENT: u64 = 50;
const MAX_PULL_BYTES: u64 = 512;
/// The replica's choices are drawn apart from the store's.
const REPLICA_SALT: u64 = 0x5eed_91a1_4a57_2026;

const EARLY_TABLE: &str = "cache";
const PLAIN_TABLE: &str = "session";
const HISTORY_TABLE: &str = "player";

type SimStore = Store<SimStorage, SimClock>;
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub trace: Vec<String>,
    /// Groups of puts answered before they were on disk.
    pub early: usize,
    /// Writes that waited for the disk, and how many of them carried
    /// puts answered earlier.
    pub waited: usize,
    pub carried: usize,
    pub restarts: usize,
    /// Restarts that lost puts answered early, and how many groups.
    pub restarts_that_lost: usize,
    pub early_lost: usize,
    /// Interrupted writes that came back whole, and that did not.
    pub in_flight_kept: usize,
    pub in_flight_lost: usize,
    /// Puts refused because the log had refused a write.
    pub refused_after_a_failure: usize,
    pub replica_pulls: usize,
    /// Writes to the key file made to fail, in a run whose values are
    /// sealed.
    pub key_faults: usize,
}

/// A write that must be on disk when it is answered.
enum Waiting {
    /// Commits and puts sent together, as a committer sends them.
    Group(Vec<Request>),
    /// A put made on the store itself, to the table that answers early
    /// through a committer.
    Direct(Vec<Edit>),
    /// The puts held are written, and nothing else.
    SyncPuts,
    Branch(String),
}

impl Waiting {
    fn name(&self) -> &'static str {
        match self {
            Self::Group(_) => "write",
            Self::Direct(_) => "put on the store",
            Self::SyncPuts => "sync puts",
            Self::Branch(_) => "branch",
        }
    }

    /// The edits it makes, in the order sent. The three tables share no
    /// key, so the order across kinds does not matter.
    fn edits(&self) -> Vec<Edit> {
        match self {
            Self::Group(requests) => edits_of(requests),
            Self::Direct(edits) => edits.clone(),
            Self::SyncPuts | Self::Branch(_) => Vec::new(),
        }
    }

    fn run(self, store: &mut SimStore) -> Result<(), StoreError> {
        match self {
            Self::Group(requests) => answers(store.write_requests(requests, true)),
            Self::Direct(edits) => store.put(edits),
            Self::SyncPuts => store.sync_puts(),
            Self::Branch(name) => store.create_branch(&name, MAIN_BRANCH),
        }
    }
}

struct Run {
    rng: Rng,
    store: SimStore,
    /// What the store held at the last write that waited for the disk.
    durable: Model,
    /// The edits of each group of puts answered since, oldest first.
    ahead: Vec<Vec<Edit>>,
    /// The edits of the write whose sync was interrupted, if its bytes
    /// may still be on the disk.
    in_flight: Option<Vec<Edit>>,
    branches: usize,
    outcome: Outcome,
    /// None when values are sealed: this run is about the key file, and
    /// a replica of a sealed store is the business of another.
    replica: Option<SimStore>,
    following: Rng,
    /// The key file, when values are sealed.
    keys: Option<SimKeyStorage>,
}

pub fn run(seed: u64) -> Result<Outcome, String> {
    run_with(seed, false)
}

/// The same run with values sealed, every key a subject of its own. A
/// put answered before it is on disk has its key held with it: the run
/// checks that the store opens again after every failure, which it does
/// only if the key file holds every key the log uses, and that the key
/// file was not written for a put answered early.
pub fn run_sealed(seed: u64) -> Result<Outcome, String> {
    run_with(seed, true)
}

fn open(
    disk: SimStorage,
    clock: SimClock,
    keys: Option<&SimKeyStorage>,
) -> Result<SimStore, StoreError> {
    let Some(keys) = keys else {
        return Store::open(disk, clock);
    };
    let storage: Box<dyn KeyStorage + Send> = Box::new(keys.clone());
    let keys = Keys::open(storage).map_err(StoreError::Keys)?;
    Store::open_sealed(disk, keys, clock)
}

fn run_with(seed: u64, is_sealed: bool) -> Result<Outcome, String> {
    let mut rng = Rng::new(seed);
    let clock = SimClock::new(&mut rng);
    let keys = is_sealed.then(SimKeyStorage::default);
    let mut store = open(SimStorage::default(), clock, keys.as_ref())
        .map_err(|e| format!("empty store fails to open: {e}"))?;
    for (table, engine) in [
        (EARLY_TABLE, Engine::PlainAsync),
        (PLAIN_TABLE, Engine::Plain),
    ] {
        store
            .declare_table(table.as_bytes(), engine, provenance(0))
            .map_err(|e| format!("declaring {table} fails: {e}"))?;
    }
    let mut following = Rng::new(seed ^ REPLICA_SALT);
    let replica = if is_sealed {
        None
    } else {
        let replica = Store::open(SimStorage::default(), SimClock::new(&mut following))
            .map_err(|e| format!("empty replica fails to open: {e}"))?;
        Some(replica)
    };
    let mut run = Run {
        rng,
        store,
        durable: Model::new(),
        ahead: Vec::new(),
        in_flight: None,
        branches: 0,
        outcome: Outcome::default(),
        replica,
        following,
        keys,
    };
    for step in 0..STEPS {
        run.step(step)?;
        run.check_reads(step)?;
        if run.following.percent(REPLICA_PULL_PERCENT) {
            let max_bytes = 1 + run.following.below(MAX_PULL_BYTES) as usize;
            run.pull(step, max_bytes)?;
        }
    }
    run.check_replica()?;
    Ok(run.outcome)
}

impl Run {
    fn step(&mut self, step: usize) -> Result<(), String> {
        let mut roll = self.rng.below(100);
        let mut is = |percent: u64| {
            let chosen = roll < percent;
            roll = roll.saturating_sub(percent);
            chosen
        };
        if is(EARLY_PERCENT) {
            return self.early(step);
        }
        let waiting = if is(WAITING_PERCENT) {
            Waiting::Group(self.random_requests())
        } else if is(DIRECT_PERCENT) {
            Waiting::Direct(self.random_edits(EARLY_TABLE))
        } else if is(SYNC_PUTS_PERCENT) {
            Waiting::SyncPuts
        } else if is(BRANCH_PERCENT) {
            self.branches += 1;
            Waiting::Branch(format!("b{}", self.branches))
        } else {
            return self.restart(step, !is(PROCESS_CRASH_PERCENT));
        };
        self.waiting(step, waiting)
    }

    /// Puts to the table that answers early, sent as a committer sends
    /// them. Nothing is written, so no sync can fail.
    fn early(&mut self, step: usize) -> Result<(), String> {
        let count = 1 + self.rng.below(MAX_REQUESTS);
        let requests: Vec<Request> = (0..count)
            .map(|_| Request::Put(self.random_edits(EARLY_TABLE)))
            .collect();
        let edits = edits_of(&requests);
        let records = self.store.log_records();
        let key_file = self.key_file_len();
        let held_by_the_reader = self.rng.percent(HELD_BY_THE_READER_PERCENT);
        if held_by_the_reader && self.keys.is_none() {
            // As the thread that read them holds them: no values are
            // sealed in this store, so they are ready as they are.
            match self.store.hold_sealed(edits.clone()) {
                Ok(None) => {}
                Ok(Some(_)) => return Err(format!("step {step}: puts were given back")),
                Err(e) => return Err(format!("step {step}: puts held early failed: {e}")),
            }
        } else {
            answers(self.store.write_requests(requests, true))
                .map_err(|e| format!("step {step}: a put answered early failed: {e}"))?;
        }
        if self.store.log_records() != records {
            return Err(format!("step {step}: a put answered early was written"));
        }
        if !self.store.has_unsynced_puts() {
            return Err(format!("step {step}: the store forgot the puts it holds"));
        }
        if self.key_file_len() != key_file {
            return Err(format!(
                "step {step}: a put answered early wrote the key file"
            ));
        }
        self.ahead.push(edits);
        self.outcome.early += 1;
        self.trace(step, "puts answered early".to_owned());
        Ok(())
    }

    /// A write that must be on disk when it is answered, sometimes with
    /// its sync made to fail.
    fn waiting(&mut self, step: usize, waiting: Waiting) -> Result<(), String> {
        let what = waiting.name();
        let edits = waiting.edits();
        let has_ahead = !self.ahead.is_empty();
        // Writing the held puts when none is held writes nothing.
        let syncs = has_ahead || !matches!(waiting, Waiting::SyncPuts);
        let fault = (syncs && self.rng.percent(SYNC_FAULT_PERCENT)).then(|| {
            if self.rng.percent(50) {
                SyncFault::KeepPending
            } else {
                SyncFault::DropPending
            }
        });
        if let Some(fault) = fault {
            self.store.storage_mut().fault_next_sync(fault);
        }
        let has_key_fault = fault.is_none() && self.maybe_key_fault();
        let written = waiting.run(&mut self.store);
        if has_key_fault {
            let was_unused = self.keys.as_ref().is_some_and(SimKeyStorage::clear_fault);
            match (&written, was_unused) {
                // No key was written, so nothing failed.
                (_, true) => {}
                (Err(StoreError::Keys(_)), false) => {
                    // The write was not made, and nothing was put in
                    // the log: what comes back is what was durable, and
                    // of the puts answered early as many as any restart
                    // keeps.
                    self.outcome.key_faults += 1;
                    self.trace(step, format!("{what}: key file fault"));
                    let power = self.rng.percent(50);
                    return self.restart(step, power);
                }
                (result, false) => {
                    return Err(format!(
                        "step {step}: {what} with a key file fault returned {result:?}"
                    ));
                }
            }
        }
        match (written, fault) {
            (Ok(()), None) => {
                if self.store.has_unsynced_puts() {
                    return Err(format!("step {step}: {what} left puts off the disk"));
                }
                for group in self.ahead.drain(..) {
                    apply(&mut self.durable, &group);
                }
                apply(&mut self.durable, &edits);
                self.outcome.waited += 1;
                self.outcome.carried += usize::from(has_ahead);
                self.trace(step, what.to_owned());
                Ok(())
            }
            (Err(StoreError::Log(LogError::Io(_))), Some(fault)) => {
                if fault == SyncFault::KeepPending {
                    self.in_flight = Some(edits);
                }
                self.trace(step, format!("{what}: sync fault {fault:?}"));
                self.check_refuses_puts(step)?;
                // A replica asking now must not be handed what failed.
                self.pull(step, usize::MAX)?;
                let power = fault == SyncFault::DropPending || self.rng.percent(50);
                self.restart(step, power)
            }
            (result, fault) => Err(format!(
                "step {step}: {what} with fault {fault:?} returned {result:?}"
            )),
        }
    }

    /// After the log refused a write, a put that would be answered early
    /// is refused too.
    fn check_refuses_puts(&mut self, step: usize) -> Result<(), String> {
        let requests = vec![Request::Put(self.random_edits(EARLY_TABLE))];
        match self.store.write_requests(requests, true) {
            Err(StoreError::Log(LogError::Poisoned)) => {
                self.outcome.refused_after_a_failure += 1;
                Ok(())
            }
            other => Err(format!(
                "step {step}: after a failed write, a put was answered {other:?}"
            )),
        }
    }

    /// The store holds everything answered, on disk or not.
    fn check_reads(&self, step: usize) -> Result<(), String> {
        let mut expected = self.durable.clone();
        for group in &self.ahead {
            apply(&mut expected, group);
        }
        if entries_of(&self.store) != expected {
            return Err(format!(
                "step {step}: a read does not see every answered write"
            ));
        }
        Ok(())
    }

    fn restart(&mut self, step: usize, power: bool) -> Result<(), String> {
        let disk = if power {
            self.store.storage_mut().power_cut(&mut self.rng)
        } else {
            self.store.storage_mut().process_crash()
        };
        let clock = SimClock::new(&mut self.rng);
        self.store = open(disk, clock, self.keys.as_ref())
            .map_err(|e| format!("step {step}: store fails to reopen: {e}"))?;
        self.outcome.restarts += 1;
        let held = entries_of(&self.store);
        let ahead = std::mem::take(&mut self.ahead);
        let in_flight = self.in_flight.take();
        // Everything answered up to some moment, no earlier than the
        // last write that waited for the disk.
        let mut moment = self.durable.clone();
        let mut kept = (held == moment).then_some(0);
        for (count, group) in ahead.iter().enumerate() {
            apply(&mut moment, group);
            if kept.is_none() && held == moment {
                kept = Some(count + 1);
            }
        }
        if let Some(edits) = &in_flight {
            apply(&mut moment, edits);
            if kept.is_none() && held == moment {
                // Whole, with every put answered before it.
                self.outcome.in_flight_kept += 1;
                self.durable = held;
                self.trace(step, "restart: the interrupted write is there".to_owned());
                return Ok(());
            }
            self.outcome.in_flight_lost += 1;
        }
        let Some(kept) = kept else {
            return Err(format!(
                "step {step}: what came back is not what was answered up to any moment \
                 since the last write that waited for the disk ({} groups of puts since)",
                ahead.len()
            ));
        };
        let lost = ahead.len() - kept;
        self.outcome.early_lost += lost;
        self.outcome.restarts_that_lost += usize::from(lost > 0);
        self.durable = held;
        self.trace(
            step,
            format!("restart: lost the last {lost} groups of puts"),
        );
        Ok(())
    }

    /// The replica asks for what comes after the records it holds. The
    /// store's log must hold those records too: nothing was handed out
    /// that a restart then took back.
    fn pull(&mut self, step: usize, max_bytes: usize) -> Result<(), String> {
        let Some(replica) = self.replica.as_mut() else {
            return Ok(());
        };
        let held = replica.log_records();
        let theirs = replica
            .log_chain(held)
            .map_err(|e| format!("step {step}: replica cannot read its log: {e}"))?;
        let ours = self
            .store
            .log_chain(held)
            .map_err(|e| format!("step {step}: store cannot read its log: {e}"))?;
        if ours.is_none() || ours != theirs {
            return Err(format!(
                "step {step}: the replica holds {held} records the store does not hold"
            ));
        }
        let frames = self
            .store
            .log_after(held, max_bytes)
            .map_err(|e| format!("step {step}: store cannot read its log: {e}"))?;
        if frames.is_empty() {
            return Ok(());
        }
        replica
            .replicate(&frames)
            .map_err(|e| format!("step {step}: replica refuses its primary's records: {e}"))?;
        self.outcome.replica_pulls += 1;
        Ok(())
    }

    /// Once the held puts are written and the replica has every record,
    /// it holds what the store holds.
    fn check_replica(&mut self) -> Result<(), String> {
        self.store
            .sync_puts()
            .map_err(|e| format!("writing the held puts at the end fails: {e}"))?;
        while self
            .replica
            .as_ref()
            .is_some_and(|replica| replica.log_records() < self.store.log_records())
        {
            self.pull(STEPS, usize::MAX)?;
        }
        if let Some(replica) = &self.replica
            && entries_of(replica) != entries_of(&self.store)
        {
            return Err("the replica holds other values".to_owned());
        }
        Ok(())
    }

    /// How long the key file is, or 0 if values are not sealed.
    fn key_file_len(&self) -> usize {
        let Some(keys) = &self.keys else {
            return 0;
        };
        keys.clone().read_all().map_or(0, |bytes| bytes.len())
    }

    /// Sometimes, when values are sealed, makes the next write to the
    /// key file fail. Returns whether it did.
    fn maybe_key_fault(&mut self) -> bool {
        let Some(keys) = &self.keys else {
            return false;
        };
        if !self.rng.percent(KEY_FAULT_PERCENT) {
            return false;
        }
        let fault = KeyFault {
            torn: self.rng.below(MAX_TORN) as usize,
            garbage: self.rng.percent(50).then(|| self.rng.next_u64()),
            new_survives: self.rng.percent(50),
        };
        keys.fail_next_write(fault);
        true
    }

    /// Commits and puts as they arrive together: to the table that keeps
    /// its history, to the one that waits, and to the one that would
    /// answer early were it alone. At least one of them waits.
    fn random_requests(&mut self) -> Vec<Request> {
        let count = 1 + self.rng.below(MAX_REQUESTS);
        let mut requests: Vec<Request> = (0..count)
            .map(|_| match self.rng.below(3) {
                0 => Request::Commit(CommitRequest {
                    branch: MAIN_BRANCH.to_owned(),
                    edits: self.random_edits(HISTORY_TABLE),
                    provenance: provenance(0),
                }),
                1 => Request::Put(self.random_edits(EARLY_TABLE)),
                _ => Request::Put(self.random_edits(PLAIN_TABLE)),
            })
            .collect();
        let waits = requests.iter().any(|request| match request {
            Request::Commit(_) => true,
            Request::Put(edits) | Request::SealedPut(edits) => {
                !edits[0].key.starts_with(EARLY_TABLE.as_bytes())
            }
        });
        if !waits {
            requests.push(Request::Put(self.random_edits(PLAIN_TABLE)));
        }
        requests
    }

    /// A few edits to distinct keys of `table`.
    fn random_edits(&mut self, table: &str) -> Vec<Edit> {
        let count = 1 + self.rng.below(MAX_EDITS);
        let edits: BTreeMap<Vec<u8>, Option<Vec<u8>>> = (0..count)
            .map(|_| {
                let key = format!("{table}/{:02}", self.rng.below(KEY_SPACE)).into_bytes();
                let value = (!self.rng.percent(REMOVE_PERCENT))
                    .then(|| self.rng.next_u64().to_le_bytes().to_vec());
                (key, value)
            })
            .collect();
        edits
            .into_iter()
            .map(|(key, value)| Edit { key, value })
            .collect()
    }

    fn trace(&mut self, step: usize, event: String) {
        self.outcome.trace.push(format!("{step}: {event}"));
    }
}

/// Whether the group was written and every request in it accepted.
fn answers<T>(written: Result<Vec<Result<T, StoreError>>, StoreError>) -> Result<(), StoreError> {
    for answer in written? {
        answer?;
    }
    Ok(())
}

fn edits_of(requests: &[Request]) -> Vec<Edit> {
    requests
        .iter()
        .flat_map(|request| request.edits().iter().cloned())
        .collect()
}

fn apply(model: &mut Model, edits: &[Edit]) {
    for edit in edits {
        match &edit.value {
            Some(value) => model.insert(edit.key.clone(), value.clone()),
            None => model.remove(&edit.key),
        };
    }
}

fn entries_of(store: &SimStore) -> Model {
    store
        .entries(MAIN_BRANCH)
        .into_iter()
        .map(|(key, value)| (key.to_vec(), value.to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEEDS_ENV: &str = "UMBRACHYX_SEEDS";
    const QUICK_SEEDS: u64 = 300;
    const FULL_SEEDS: u64 = 10_000;

    fn run_seeds(seeds: u64) -> Outcome {
        run_seeds_of(seeds, run)
    }

    fn run_seeds_of(seeds: u64, run: fn(u64) -> Result<Outcome, String>) -> Outcome {
        let mut total = Outcome::default();
        for seed in 0..seeds {
            let outcome = run(seed).unwrap_or_else(|error| {
                panic!(
                    "seed {seed}: {error}\nreplay: UMBRACHYX_SEED={seed} cargo test -p umbrachyx \
                     --lib sim::plain_run::tests::replay_seed -- --ignored --exact --nocapture"
                )
            });
            total.early += outcome.early;
            total.waited += outcome.waited;
            total.carried += outcome.carried;
            total.restarts += outcome.restarts;
            total.restarts_that_lost += outcome.restarts_that_lost;
            total.early_lost += outcome.early_lost;
            total.in_flight_kept += outcome.in_flight_kept;
            total.in_flight_lost += outcome.in_flight_lost;
            total.refused_after_a_failure += outcome.refused_after_a_failure;
            total.replica_pulls += outcome.replica_pulls;
            total.key_faults += outcome.key_faults;
        }
        total
    }

    #[test]
    fn same_seed_runs_the_same_way_sealed() {
        let outcome = |seed| run_sealed(seed).map(|outcome| outcome.trace);
        assert_eq!(outcome(42), outcome(42));
    }

    /// Sealed, the promises are only tested if keys are held, restarts
    /// lose puts and the key file fails.
    #[test]
    fn faults_actually_happen_sealed() {
        let total = run_seeds_of(QUICK_SEEDS, run_sealed);

        assert!(total.early >= 5_000, "{total:?}");
        assert!(total.waited >= 2_000, "{total:?}");
        assert!(total.restarts >= 2_000, "{total:?}");
        assert!(total.restarts_that_lost >= 500, "{total:?}");
        assert!(total.key_faults >= 100, "{total:?}");
    }

    #[test]
    #[ignore = "10,000 seeds; run with --ignored"]
    fn ten_thousand_seeds_sealed_plain() {
        let seeds = std::env::var(SEEDS_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(FULL_SEEDS);
        let total = run_seeds_of(seeds, run_sealed);
        println!(
            "plain tables with sealed values, {seeds} seeds: {} groups of puts answered before \
             they or their keys were on disk, {} writes that waited for it, {} restarts, {} of \
             them lost the last puts ({} groups in all, never anything else), {} writes to the \
             key file failed; the store opened again every time",
            total.early,
            total.waited,
            total.restarts,
            total.restarts_that_lost,
            total.early_lost,
            total.key_faults,
        );
    }

    #[test]
    fn same_seed_runs_the_same_way() {
        assert_eq!(run(42), run(42));
    }

    /// The promises are only tested if the faults happen.
    #[test]
    fn faults_actually_happen() {
        let total = run_seeds(QUICK_SEEDS);

        assert!(total.early >= 5_000, "{total:?}");
        assert!(total.waited >= 3_000, "{total:?}");
        assert!(total.carried >= 1_000, "{total:?}");
        assert!(total.restarts >= 2_000, "{total:?}");
        assert!(total.restarts_that_lost >= 500, "{total:?}");
        assert!(total.in_flight_kept >= 20, "{total:?}");
        assert!(total.in_flight_lost >= 20, "{total:?}");
        assert!(total.refused_after_a_failure >= 100, "{total:?}");
        assert!(total.replica_pulls >= 3_000, "{total:?}");
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
            "plain tables, {seeds} seeds: {} groups of puts answered before they were on disk, \
             {} writes that waited for it ({} carried earlier puts), {} restarts, {} of them \
             lost the last puts ({} groups in all, never anything else), interrupted writes \
             kept {} and lost {}, {} puts refused after a failed write; a replica was sent \
             records {} times",
            total.early,
            total.waited,
            total.carried,
            total.restarts,
            total.restarts_that_lost,
            total.early_lost,
            total.in_flight_kept,
            total.in_flight_lost,
            total.refused_after_a_failure,
            total.replica_pulls,
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
