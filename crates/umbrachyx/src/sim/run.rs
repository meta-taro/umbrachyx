//! One simulated run of the log: writes, failed syncs, power cuts and bit
//! flips, all chosen by a seed, with the log's promises checked after every
//! restart.
//!
//! The promises:
//! - every acknowledged record comes back, in order and unchanged;
//! - at most one more record comes back, and only the one whose sync was
//!   interrupted;
//! - damage to an acknowledged record before the last one stops the log from
//!   opening instead of dropping records.
//!
//! Damage inside the very last acknowledged record cannot be told apart from
//! a torn write, so it may be cut off; the run counts those.

use std::hash::{DefaultHasher, Hash, Hasher};

use super::disk::{SimStorage, SyncFault};
use super::rng::Rng;
use crate::log::{Branch, Commit, Edit, Log, LogError, Record, Storage, record};

const STEPS: usize = 64;

/// Out of 100, per step; the rest flip a bit and cut power.
const APPEND_PERCENT: u64 = 80;
const PROCESS_CRASH_PERCENT: u64 = 8;
const POWER_CUT_PERCENT: u64 = 10;
/// Out of 100, per append.
const SYNC_POWER_CUT_PERCENT: u64 = 4;
const SYNC_DROP_PERCENT: u64 = 2;
/// Out of 100, after a failed sync: try to write again on the same log.
const WRITE_AFTER_FAILURE_PERCENT: u64 = 50;
/// Out of 100, after a successful open: cut power again before any write.
const SECOND_POWER_CUT_PERCENT: u64 = 25;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Every event in order, with hashes of the disk and of what was
    /// recovered, so two runs of one seed can be compared.
    pub trace: Vec<String>,
    pub acknowledged: usize,
    pub restarts: usize,
    /// Damage inside the last acknowledged record, cut off as a torn write.
    pub lost_to_last_record_damage: usize,
    /// Damage before the last record, refused at open as it must be.
    pub detected_corruption: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crash {
    /// The process dies; the kernel keeps what was written.
    Process,
    /// The machine loses power, optionally with a bit flipped on disk.
    Power { flip: bool },
}

struct Run {
    rng: Rng,
    log: Log<SimStorage>,
    acknowledged: Vec<Record>,
    /// A record whose sync was interrupted: it may or may not survive.
    in_flight: Option<Record>,
    outcome: Outcome,
}

/// Run one seed. An `Err` is a broken promise, with what was expected.
pub fn run(seed: u64) -> Result<Outcome, String> {
    let (log, recovered) =
        Log::open(SimStorage::default()).map_err(|e| format!("empty log fails to open: {e}"))?;
    if !recovered.is_empty() {
        return Err("an empty log returned records".to_owned());
    }
    let mut run = Run {
        rng: Rng::new(seed),
        log,
        acknowledged: Vec::new(),
        in_flight: None,
        outcome: Outcome::default(),
    };
    for step in 0..STEPS {
        let is_open = run.step(step)?;
        if !is_open {
            break;
        }
    }
    run.outcome.acknowledged = run.acknowledged.len();
    Ok(run.outcome)
}

impl Run {
    /// Returns false once the log has refused to open for good.
    fn step(&mut self, step: usize) -> Result<bool, String> {
        let roll = self.rng.below(100);
        if roll < APPEND_PERCENT {
            return self.append(step);
        }
        let crash = if roll < APPEND_PERCENT + PROCESS_CRASH_PERCENT {
            Crash::Process
        } else {
            let flip = roll >= APPEND_PERCENT + PROCESS_CRASH_PERCENT + POWER_CUT_PERCENT;
            Crash::Power { flip }
        };
        self.restart(step, crash)
    }

    fn append(&mut self, step: usize) -> Result<bool, String> {
        let record = random_record(&mut self.rng);
        let fault = if self.rng.percent(SYNC_POWER_CUT_PERCENT) {
            Some(SyncFault::KeepPending)
        } else if self.rng.percent(SYNC_DROP_PERCENT) {
            Some(SyncFault::DropPending)
        } else {
            None
        };
        if let Some(fault) = fault {
            self.log.storage_mut().fault_next_sync(fault);
        }

        match (self.log.append(&record), fault) {
            (Ok(()), None) => {
                self.trace(step, format!("ack {}", describe(&record)));
                self.acknowledged.push(record);
                self.check_len(step)?;
                Ok(true)
            }
            (Err(LogError::Io(_)), Some(fault)) => {
                self.trace(step, format!("sync fault {fault:?}"));
                if fault == SyncFault::KeepPending {
                    self.in_flight = Some(record);
                }
                self.write_after_failure(step)?;
                let crash = match fault {
                    SyncFault::KeepPending if self.rng.percent(50) => Crash::Process,
                    _ => Crash::Power { flip: false },
                };
                self.restart(step, crash)
            }
            (result, fault) => Err(format!(
                "step {step}: append with fault {fault:?} returned {result:?}"
            )),
        }
    }

    /// After a failed sync the log must refuse further writes: retrying
    /// could acknowledge bytes the failed sync already lost.
    fn write_after_failure(&mut self, step: usize) -> Result<(), String> {
        if !self.rng.percent(WRITE_AFTER_FAILURE_PERCENT) {
            return Ok(());
        }
        let record = random_record(&mut self.rng);
        match self.log.append(&record) {
            Err(LogError::Poisoned) => {
                self.trace(step, "write after failure refused".to_owned());
                Ok(())
            }
            other => Err(format!(
                "step {step}: write after a failed sync returned {other:?}"
            )),
        }
    }

    fn restart(&mut self, step: usize, crash: Crash) -> Result<bool, String> {
        let mut disk = match crash {
            Crash::Process => self.log.storage_mut().process_crash(),
            Crash::Power { .. } => self.log.storage_mut().power_cut(&mut self.rng),
        };
        let damaged = match crash {
            Crash::Power { flip: true } => self.flip_a_bit(&mut disk),
            _ => None,
        };
        self.outcome.restarts += 1;
        let disk_hash = hash(&disk.read_all().unwrap_or_default());
        self.trace(
            step,
            format!("{crash:?}, disk {disk_hash:016x}, damaged {damaged:?}"),
        );

        match (Log::open(disk), damaged) {
            (Ok((log, recovered)), _) => {
                self.check_recovered(step, &recovered, damaged)?;
                self.log = log;
                self.acknowledged = recovered;
                self.in_flight = None;
                self.check_len(step)?;
                if self.rng.percent(SECOND_POWER_CUT_PERCENT) {
                    // What open handed out must already be durable.
                    return self.restart(step, Crash::Power { flip: false });
                }
                Ok(true)
            }
            (Err(LogError::Corrupt { offset, .. }), Some(index)) => {
                self.check_refusal(step, index, offset)?;
                self.trace(step, format!("refused to open: corrupt at {offset}"));
                self.outcome.detected_corruption = true;
                Ok(false)
            }
            (Err(error), _) => Err(format!(
                "step {step}: open failed with {error} (damaged record {damaged:?})"
            )),
        }
    }

    /// Flip one bit in the bytes of an acknowledged record and return its
    /// index. Bits past the acknowledged records are left alone: those bytes
    /// were never promised.
    fn flip_a_bit(&mut self, disk: &mut SimStorage) -> Option<usize> {
        let ends = frame_ends(&self.acknowledged);
        let acknowledged_len = *ends.last()?;
        let bit = self.rng.below(acknowledged_len as u64 * 8) as usize;
        disk.flip_bit(bit);
        ends.iter().position(|&end| bit / 8 < end)
    }

    fn check_recovered(
        &mut self,
        step: usize,
        recovered: &[Record],
        damaged: Option<usize>,
    ) -> Result<(), String> {
        let last = self.acknowledged.len().checked_sub(1);
        let kept = match damaged {
            None => self.acknowledged.len(),
            Some(index) if Some(index) == last => {
                self.outcome.lost_to_last_record_damage += 1;
                index
            }
            Some(index) => {
                return Err(format!(
                    "step {step}: record {index} of {} was damaged and the log still opened",
                    self.acknowledged.len()
                ));
            }
        };
        let expected = &self.acknowledged[..kept];
        if recovered.get(..kept) != Some(expected) {
            return Err(format!(
                "step {step}: {} acknowledged records expected back, got {} that differ",
                kept,
                recovered.len()
            ));
        }
        match &recovered[kept..] {
            [] => {}
            [extra] if damaged.is_none() && self.in_flight.as_ref() == Some(extra) => {
                self.trace(step, "interrupted record survived".to_owned());
            }
            extra => {
                return Err(format!(
                    "step {step}: {} records came back that were never acknowledged",
                    extra.len()
                ));
            }
        }
        self.trace(
            step,
            format!("recovered {}, {:016x}", recovered.len(), hash(recovered)),
        );
        Ok(())
    }

    /// Refusing to open is right only for damage before the last record, or
    /// in the last one when a whole record follows it; and the reported
    /// offset must be where the damaged record starts.
    fn check_refusal(&self, step: usize, index: usize, offset: u64) -> Result<(), String> {
        let is_last = index + 1 == self.acknowledged.len();
        if is_last && self.in_flight.is_none() {
            return Err(format!(
                "step {step}: refused to open over damage in the last record, \
                 which should have been cut as a torn write"
            ));
        }
        let start = match index {
            0 => 0,
            _ => frame_ends(&self.acknowledged)[index - 1],
        };
        if offset != start as u64 {
            return Err(format!(
                "step {step}: corruption reported at byte {offset}, \
                 but damaged record {index} starts at {start}"
            ));
        }
        Ok(())
    }

    fn check_len(&mut self, step: usize) -> Result<(), String> {
        let expected = frame_ends(&self.acknowledged).last().copied().unwrap_or(0);
        if self.log.len() != expected as u64 || self.log.is_empty() != (expected == 0) {
            return Err(format!(
                "step {step}: log reports {} bytes, acknowledged records take {expected}",
                self.log.len()
            ));
        }
        Ok(())
    }

    fn trace(&mut self, step: usize, event: String) {
        self.outcome.trace.push(format!("{step}: {event}"));
    }
}

/// Same input, same hash, for as long as the toolchain is pinned.
fn hash(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Byte offset where each record's frame ends, in log order.
fn frame_ends(records: &[Record]) -> Vec<usize> {
    records
        .iter()
        .scan(0, |end, r| {
            *end += record::encode(r).map_or(0, |frame| frame.len());
            Some(*end)
        })
        .collect()
}

/// Mostly commits of one small write, sometimes several writes, removes,
/// large values that span many sectors, or a new branch.
fn random_record(rng: &mut Rng) -> Record {
    if rng.percent(5) {
        let name_len = 1 + rng.below(24) as usize;
        let from_len = 1 + rng.below(24) as usize;
        return Record::Branch(Branch {
            name: rng.ascii(name_len),
            from: rng.ascii(from_len),
        });
    }
    let count = if rng.percent(80) { 1 } else { 1 + rng.below(8) };
    let changes = (0..count).map(|_| random_write(rng)).collect();
    let who_len = 1 + rng.below(16) as usize;
    let why_len = rng.below(48) as usize;
    let approver_len = 1 + rng.below(16) as usize;
    let branch_len = 1 + rng.below(12) as usize;
    Record::Commit(Commit {
        branch: rng.ascii(branch_len),
        changes,
        who: rng.ascii(who_len),
        why: rng.ascii(why_len),
        approved_by: rng.percent(30).then(|| rng.ascii(approver_len)),
        time: rng.next_u64(),
    })
}

fn random_write(rng: &mut Rng) -> Edit {
    let value_len = match rng.below(100) {
        0..90 => rng.below(256),
        90..99 => rng.below(4_096),
        _ => rng.below(65_536),
    } as usize;
    let key_len = 1 + rng.below(32) as usize;
    Edit {
        key: rng.bytes(key_len),
        value: (!rng.percent(10)).then(|| rng.bytes(value_len)),
    }
}

fn describe(record: &Record) -> String {
    let Record::Commit(record) = record else {
        return "new branch".to_owned();
    };
    let bytes: usize = record
        .changes
        .iter()
        .map(|w| w.key.len() + w.value.as_ref().map_or(0, Vec::len))
        .sum();
    format!("{} writes, {bytes} B", record.changes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED_ENV: &str = "UMBRACHYX_SEED";
    const SEEDS_ENV: &str = "UMBRACHYX_SEEDS";
    const QUICK_SEEDS: u64 = 300;
    const FULL_SEEDS: u64 = 10_000;

    fn reproduce(seed: u64) -> String {
        format!(
            "reproduce: {SEED_ENV}={seed} cargo test -p umbrachyx --lib \
             sim::run::tests::replay_seed -- --ignored --exact --nocapture"
        )
    }

    fn run_seeds(count: u64) -> Vec<Outcome> {
        (0..count)
            .map(|seed| {
                run(seed)
                    .unwrap_or_else(|broken| panic!("seed {seed}: {broken}\n{}", reproduce(seed)))
            })
            .collect()
    }

    fn summary(outcomes: &[Outcome]) -> String {
        let total = |f: fn(&Outcome) -> usize| outcomes.iter().map(f).sum::<usize>();
        format!(
            "{} seeds: {} restarts, {} records acknowledged at the end, \
             {} corruptions refused, {} last-record damages cut off",
            outcomes.len(),
            total(|o| o.restarts),
            total(|o| o.acknowledged),
            total(|o| usize::from(o.detected_corruption)),
            total(|o| o.lost_to_last_record_damage),
        )
    }

    #[test]
    fn same_seed_runs_the_same_way() {
        for seed in [0, 1, 42, 0xDEAD_BEEF] {
            let first = run(seed).expect("runs");
            let second = run(seed).expect("runs");

            assert_eq!(first, second, "seed {seed}");
        }
    }

    #[test]
    fn faults_actually_happen() {
        let outcomes = run_seeds(QUICK_SEEDS);
        let count = |needle: &str| {
            outcomes
                .iter()
                .flat_map(|o| &o.trace)
                .filter(|e| e.contains(needle))
                .count()
        };
        let corruptions = outcomes.iter().filter(|o| o.detected_corruption).count();
        let last_record_cuts: usize = outcomes.iter().map(|o| o.lost_to_last_record_damage).sum();
        let report = format!(
            "{}; process crashes {}, survived {}, refused writes {}",
            summary(&outcomes),
            count("Process,"),
            count("interrupted record survived"),
            count("write after failure refused"),
        );

        // Each floor is well below what the 300 seeds produce today, and
        // well above zero: a fault path that silently stops firing fails here.
        assert!(count("sync fault KeepPending") >= 100, "{report}");
        assert!(count("sync fault DropPending") >= 50, "{report}");
        assert!(count("Process,") >= 500, "{report}");
        assert!(count("interrupted record survived") >= 30, "{report}");
        assert!(count("write after failure refused") >= 50, "{report}");
        assert!(corruptions >= 30, "{report}");
        assert!(last_record_cuts >= 5, "{report}");
    }

    #[test]
    #[ignore = "10,000 seeds; run with --ignored"]
    fn ten_thousand_seeds() {
        let count = std::env::var(SEEDS_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(FULL_SEEDS);

        let outcomes = run_seeds(count);

        println!("{}", summary(&outcomes));
    }

    #[test]
    #[ignore = "replays the seed in UMBRACHYX_SEED"]
    fn replay_seed() {
        let Ok(seed) = std::env::var(SEED_ENV) else {
            println!("set {SEED_ENV} to the seed to replay");
            return;
        };
        let seed = seed.parse().expect("UMBRACHYX_SEED is a number");

        let outcome = run(seed).unwrap_or_else(|broken| panic!("seed {seed}: {broken}"));

        for event in &outcome.trace {
            println!("{event}");
        }
    }
}
