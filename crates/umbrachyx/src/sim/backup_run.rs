//! One simulated run of a store that is backed up while it is written to:
//! commits and erasures, backups that run to their end and backups that
//! are stopped after some of their writes, with the power cut or the
//! process killed where the backup is kept, all chosen by a seed. After
//! every backup, finished or not, the backup is restored into a new store.
//!
//! The promises, checked at every restore:
//! - a backup that has finished once restores, every time after;
//! - what it restores to is the store as it was when a backup last
//!   finished, or as it was when the backup that was stopped started,
//!   whole, never a mix of the two;
//! - a subject erased before that moment reads as erased, and none of its
//!   earlier values can be read in any history;
//! - once a backup has finished, no file of it holds the key of a
//!   subject erased before it;
//! - where no backup has finished there is none to restore, rather than
//!   part of one.
//!
//! Faults of the store's own disks are the other runs' business.

use std::collections::BTreeMap;
use std::io;

use super::disk::SimStorage;
use super::key_disk::SimKeyStorage;
use super::rng::Rng;
use super::store_run::{SimClock, provenance};
use crate::keys::{Key, KeyStorage, Keys};
use crate::log::{Edit, Storage};
use crate::store::backup::{BackupError, Source, Target, restore};
use crate::store::{MAIN_BRANCH, Store};

const STEPS: usize = 48;
/// Out of 100, per step; the rest restart the store.
const COMMIT_PERCENT: u64 = 52;
const ERASE_PERCENT: u64 = 14;
const BACKUP_PERCENT: u64 = 28;
/// Out of 100, per backup: it is stopped after some of its writes.
const STOPPED_PERCENT: u64 = 60;
/// A backup makes at most this many writes: stop it before any of them.
const MAX_WRITES: u64 = 9;
/// Few subjects and fields, so erasures keep meeting live values and
/// commits keep coming back to erased subjects.
const SUBJECTS: u64 = 5;
const FIELDS: u64 = 3;
const MAX_EDITS: u64 = 3;
/// Out of 100, per edit.
const REMOVE_PERCENT: u64 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Held {
    Value(Vec<u8>),
    Erased,
}

/// What each key should read as. Keys never written, or last removed, are
/// absent.
type Model = BTreeMap<Vec<u8>, Held>;
type SealedStore = Store<SimStorage, SimClock>;

/// The store at one moment: what a backup taken then restores to.
#[derive(Clone, PartialEq, Eq, Default)]
struct Moment {
    model: Model,
    records: u64,
    /// Every value erased by then, by key, which must never read again.
    erased_values: Vec<(Vec<u8>, Vec<u8>)>,
    /// The keys destroyed by then.
    erased_keys: Vec<Key>,
}

/// Never the keys themselves.
impl std::fmt::Debug for Moment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Moment")
            .field("model", &self.model)
            .field("records", &self.records)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub trace: Vec<String>,
    pub commits: usize,
    pub erasures: usize,
    pub backups_finished: usize,
    pub backups_stopped: usize,
    pub power_cuts: usize,
    /// Restores of a stopped backup that gave the store as it was at the
    /// backup before, and as it was when the stopped one started.
    pub restored_to_before: usize,
    pub restored_to_now: usize,
    /// Restores refused because no backup had finished yet.
    pub no_backup_yet: usize,
    /// Restores that held a subject erased before the backup.
    pub restores_with_erasures: usize,
}

/// Where the backup is kept: a log that loses and tears what was not
/// synced, and files that are replaced whole. A backup can be stopped
/// after a number of writes; every write after that fails.
#[derive(Debug, Default)]
struct SimTarget {
    log: SimStorage,
    files: BTreeMap<String, Vec<u8>>,
    /// Writes the backup may still make, or `None` for all it wants.
    writes_left: Option<u64>,
    /// Whether the replacement that is refused gets through all the
    /// same, as a rename can before the error is seen.
    new_survives: bool,
}

impl SimTarget {
    /// Whether the next write may happen.
    fn write(&mut self) -> io::Result<()> {
        match &mut self.writes_left {
            Some(0) => Err(io::Error::other("injected: the backup is stopped")),
            Some(left) => {
                *left -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl Source for SimTarget {
    fn log_len(&mut self) -> io::Result<u64> {
        Ok(self.log.read_all()?.len() as u64)
    }

    fn log_read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.log.read_at(offset, len)
    }

    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.files.get(name).cloned())
    }
}

impl Target for SimTarget {
    fn log_truncate(&mut self, len: u64) -> io::Result<()> {
        self.write()?;
        self.log.truncate(len)
    }

    fn log_append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write()?;
        self.log.append(bytes)
    }

    fn log_sync(&mut self) -> io::Result<()> {
        self.write()?;
        self.log.sync()
    }

    fn replace(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let written = self.write();
        if written.is_ok() || self.new_survives {
            self.files.insert(name.to_owned(), bytes.to_vec());
        }
        written
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        self.write()?;
        self.files.remove(name);
        Ok(())
    }

    fn names(&mut self) -> io::Result<Vec<String>> {
        Ok(self.files.keys().cloned().collect())
    }
}

struct Run {
    rng: Rng,
    key_disk: SimKeyStorage,
    store: SealedStore,
    target: SimTarget,
    /// The store as it is.
    now: Moment,
    /// The store as it was when a backup last finished.
    backed_up: Option<Moment>,
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
        target: SimTarget::default(),
        now: Moment::default(),
        backed_up: None,
        outcome: Outcome::default(),
    };
    for step in 0..STEPS {
        run.step(step)?;
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
        } else if roll < COMMIT_PERCENT + ERASE_PERCENT + BACKUP_PERCENT {
            self.back_up(step)?;
            self.restore(step)
        } else {
            self.restart(step)
        }
    }

    fn commit(&mut self, step: usize) -> Result<(), String> {
        let edits = self.random_edits();
        self.store
            .commit(MAIN_BRANCH, edits.clone(), provenance(step))
            .map_err(|e| format!("step {step}: commit failed: {e}"))?;
        for edit in edits {
            match edit.value {
                Some(value) => self.now.model.insert(edit.key, Held::Value(value)),
                None => self.now.model.remove(&edit.key),
            };
        }
        self.now.records = self.store.log_records();
        self.outcome.commits += 1;
        self.trace(step, "commit".to_owned());
        Ok(())
    }

    fn erase(&mut self, step: usize) -> Result<(), String> {
        let subject = format!("p/{}", self.rng.below(SUBJECTS)).into_bytes();
        let key = Keys::open(self.key_disk.clone())
            .map_err(|e| format!("step {step}: the key file does not open: {e}"))?
            .get(&subject)
            .copied();
        let had_key = self
            .store
            .erase(&subject, provenance(step))
            .map_err(|e| format!("step {step}: erase failed: {e}"))?;
        if had_key != key.is_some() {
            return Err(format!(
                "step {step}: erase said {had_key}, the key file not"
            ));
        }
        self.now.erased_keys.extend(key);
        for (key, held) in &mut self.now.model {
            if subject_of(key) != subject {
                continue;
            }
            if let Held::Value(value) = held {
                self.now.erased_values.push((key.clone(), value.clone()));
                *held = Held::Erased;
            }
        }
        self.now.records = self.store.log_records();
        self.outcome.erasures += usize::from(had_key);
        self.trace(
            step,
            format!("erase {}: {had_key}", String::from_utf8_lossy(&subject)),
        );
        Ok(())
    }

    /// Backs the store up, sometimes stopped after some of its writes,
    /// and then cuts the power or kills the process where the backup is
    /// kept.
    fn back_up(&mut self, step: usize) -> Result<(), String> {
        let stopped = self.rng.percent(STOPPED_PERCENT);
        self.target.writes_left = stopped.then(|| self.rng.below(MAX_WRITES));
        self.target.new_survives = self.rng.percent(50);
        let result = self.store.backup(&mut self.target);
        let ran_out = self.target.writes_left == Some(0);
        self.target.writes_left = None;
        let power = self.rng.percent(50);
        self.target.log = if power {
            self.outcome.power_cuts += 1;
            self.target.log.power_cut(&mut self.rng)
        } else {
            self.target.log.process_crash()
        };
        match result {
            Ok(backup) => {
                if backup.records != self.now.records {
                    return Err(format!(
                        "step {step}: backed up {} records of {}",
                        backup.records, self.now.records
                    ));
                }
                self.backed_up = Some(self.now.clone());
                self.outcome.backups_finished += 1;
                self.trace(step, format!("backup, then power cut: {power}"));
                self.no_erased_key_is_kept(step)
            }
            Err(BackupError::Io(_)) if ran_out => {
                self.outcome.backups_stopped += 1;
                self.trace(step, format!("backup stopped, then power cut: {power}"));
                Ok(())
            }
            Err(error) => Err(format!("step {step}: backup failed: {error}")),
        }
    }

    /// Restores the backup into a new store and checks what it holds.
    fn restore(&mut self, step: usize) -> Result<(), String> {
        let mut disk = SimStorage::default();
        let mut key_disk = SimKeyStorage::default();
        match restore(&mut self.target, &mut disk, Some(&mut key_disk)) {
            Ok(_) => {}
            Err(BackupError::NoBackup) if self.backed_up.is_none() => {
                self.outcome.no_backup_yet += 1;
                return Ok(());
            }
            Err(error) => return Err(format!("step {step}: restore failed: {error}")),
        }
        let clock = SimClock::new(&mut self.rng);
        let restored = open(disk, &key_disk, clock)
            .map_err(|e| format!("step {step}: the restored store fails to open: {e}"))?;
        let held = (read_model(&restored), restored.log_records());
        let is = |moment: &Moment| (&moment.model, moment.records) == (&held.0, held.1);
        let moment = if self.backed_up.as_ref().is_some_and(is) {
            self.outcome.restored_to_before += 1;
            self.backed_up.clone().expect("checked")
        } else if is(&self.now) {
            // The stopped backup had replaced the manifest: it is the
            // backup from here on.
            self.outcome.restored_to_now += 1;
            self.backed_up = Some(self.now.clone());
            self.now.clone()
        } else {
            return Err(format!(
                "step {step}: restored {held:?}, which is neither the backup before {:?} nor \
                 the store now {:?}",
                self.backed_up, self.now
            ));
        };
        self.outcome.restores_with_erasures += usize::from(!moment.erased_values.is_empty());
        no_erased_value_reads(&restored, &moment)
            .map_err(|key| format!("step {step}: an erased value of {key} reads after a restore"))
    }

    /// A finished backup holds no key that was destroyed before it.
    fn no_erased_key_is_kept(&self, step: usize) -> Result<(), String> {
        for (name, bytes) in &self.target.files {
            for key in &self.now.erased_keys {
                if bytes.windows(key.len()).any(|window| window == key) {
                    return Err(format!(
                        "step {step}: {name} of the backup holds an erased key"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The store's process dies and it opens again; nothing is lost, since
    /// every write here was synced.
    fn restart(&mut self, step: usize) -> Result<(), String> {
        let disk = self.store.storage_mut().process_crash();
        let clock = SimClock::new(&mut self.rng);
        self.store = open(disk, &self.key_disk, clock)
            .map_err(|e| format!("step {step}: store fails to reopen: {e}"))?;
        if read_model(&self.store) != self.now.model {
            return Err(format!(
                "step {step}: the store reads otherwise after a restart"
            ));
        }
        self.trace(step, "restart".to_owned());
        Ok(())
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

fn read_model(store: &SealedStore) -> Model {
    all_keys()
        .filter_map(|key| {
            let cell = store.cell(MAIN_BRANCH, &key)?;
            let held = match (cell.value, cell.erased) {
                (Some(value), _) => Held::Value(value.to_vec()),
                (None, true) => Held::Erased,
                (None, false) => return None,
            };
            Some((key, held))
        })
        .collect()
}

/// Checks that no value erased by `moment` reads in any history of
/// `store`; the error names the key of one that does.
fn no_erased_value_reads(store: &SealedStore, moment: &Moment) -> Result<(), String> {
    for key in all_keys() {
        let revealed = store
            .history(MAIN_BRANCH, &key)
            .into_iter()
            .filter_map(|cell| cell.value);
        for value in revealed {
            if moment
                .erased_values
                .contains(&(key.clone(), value.to_vec()))
            {
                return Err(String::from_utf8_lossy(&key).into_owned());
            }
        }
    }
    Ok(())
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
    const SEED_ENV: &str = "UMBRACHYX_SEED";
    const QUICK_SEEDS: u64 = 300;
    const FULL_SEEDS: u64 = 10_000;

    fn run_seeds(seeds: u64) -> Outcome {
        let mut total = Outcome::default();
        for seed in 0..seeds {
            let outcome = run(seed).unwrap_or_else(|error| {
                panic!(
                    "seed {seed}: {error}\nreplay: UMBRACHYX_SEED={seed} cargo test -p umbrachyx \
                     --lib sim::backup_run::tests::replay_seed -- --ignored --exact --nocapture"
                )
            });
            total.commits += outcome.commits;
            total.erasures += outcome.erasures;
            total.backups_finished += outcome.backups_finished;
            total.backups_stopped += outcome.backups_stopped;
            total.power_cuts += outcome.power_cuts;
            total.restored_to_before += outcome.restored_to_before;
            total.restored_to_now += outcome.restored_to_now;
            total.no_backup_yet += outcome.no_backup_yet;
            total.restores_with_erasures += outcome.restores_with_erasures;
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

        assert!(total.backups_finished >= 1_000, "{total:?}");
        assert!(total.backups_stopped >= 1_000, "{total:?}");
        assert!(total.power_cuts >= 1_000, "{total:?}");
        assert!(total.erasures >= 500, "{total:?}");
        assert!(total.restored_to_before >= 1_000, "{total:?}");
        assert!(total.restored_to_now >= 50, "{total:?}");
        assert!(total.no_backup_yet >= 50, "{total:?}");
        assert!(total.restores_with_erasures >= 500, "{total:?}");
    }

    #[test]
    #[ignore = "10,000 seeds; run with --ignored"]
    fn ten_thousand_seeds_backup() {
        let seeds = std::env::var(SEEDS_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(FULL_SEEDS);
        let total = run_seeds(seeds);
        println!(
            "backup, {seeds} seeds: {} commits, {} erasures, {} backups finished and {} \
             stopped part way, {} power cuts where the backup is kept; restored to the \
             backup before {} times and to the stopped backup's own moment {}, {} with \
             erased subjects in it; {} restores refused before any backup had finished",
            total.commits,
            total.erasures,
            total.backups_finished,
            total.backups_stopped,
            total.power_cuts,
            total.restored_to_before,
            total.restored_to_now,
            total.restores_with_erasures,
            total.no_backup_yet
        );
    }

    #[test]
    #[ignore = "replays one seed; set UMBRACHYX_SEED"]
    fn replay_seed() {
        let seed: u64 = std::env::var(SEED_ENV)
            .expect("set UMBRACHYX_SEED")
            .parse()
            .expect("a number");
        match run(seed) {
            Ok(outcome) => println!("{}", outcome.trace.join("\n")),
            Err(error) => panic!("seed {seed}: {error}"),
        }
    }
}
