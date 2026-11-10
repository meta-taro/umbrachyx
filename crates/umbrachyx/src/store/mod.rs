//! The store: the log is the truth, and commits, branches and their trees
//! are rebuilt from it.
//!
//! A commit is acknowledged once its record is synced to the log; only then
//! is it applied. Opening the store replays the log, so nothing the store
//! holds can be missing from the log.
//!
//! Every commit keeps the cells it wrote and the cells they replaced; one
//! in `TREE_EVERY` along each line of history also keeps its whole tree,
//! and the tree of any other is rebuilt from the nearest one before it.
//! Keeping a tree per commit would copy, for every commit, the path from
//! the root to each key it changed. A branch is a name for a commit, and
//! making one copies nothing.
//!
//! Each value in the tree is a cell: the value, or a mark that it was
//! removed, and the commit that last changed it. That commit kept the cell
//! it replaced, which gives the change before, so the history of a key
//! costs the number of times it changed, not the number of commits.
//!
//! A store opened with a key file seals every value under its subject's key
//! as it comes in and opens it as it goes out (see `seal`), so the log and
//! the tree hold no value in the clear.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use crate::keys::{Fingerprint, KeyError, KeyStorage, Keys};
use crate::log::{Branch, Commit, Edit, Engine, Erase, Log, LogError, Record, Storage, Table};
use crate::tree::{Bytes, Tree};

mod committer;
mod inspect;
mod merge;
mod seal;
mod tables;
mod view;

use seal::{Ring, Sealer};
use tables::{Refusal, Tables};

pub use committer::{Committer, Pending, Reader, Writer};
pub use inspect::Inspect;
pub use merge::{Conflict, Merged, Reverted, ValueChange};
pub use view::View;

/// The branch that holds live data. It exists from the start.
pub const MAIN_BRANCH: &str = "main";
/// Branch names are short labels, not data.
const MAX_BRANCH_NAME: usize = 255;
/// The shortest start of a commit id `resolve` accepts.
pub const MIN_ID_PREFIX: usize = 4;

/// Names a commit: a BLAKE3 hash of its parent, branch, provenance, time
/// and edits.
pub type CommitId = [u8; 32];

/// Where the store reads the time. Tests replace it with a clock a seed
/// controls, the same way they replace the disk.
pub trait Clock {
    /// Microseconds since the Unix epoch. May go backwards; the store never
    /// records a time earlier than one it has already recorded.
    fn now_micros(&mut self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_micros(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
            })
    }
}

/// Who made a commit, why, and who approved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub who: String,
    pub why: String,
    pub approved_by: Option<String>,
}

/// One commit asked of [`Store::commit_all`].
#[derive(Debug, Clone)]
pub struct CommitRequest {
    pub branch: String,
    pub edits: Vec<Edit>,
    pub provenance: Provenance,
}

/// A key's value at some commit, and the commit that last changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// `None` when the change was a removal, or the value was erased.
    pub value: Option<Bytes>,
    pub changed_by: CommitId,
    /// A value was written here, and its subject has since been erased.
    pub erased: bool,
}

/// A subject erased, as the log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Erasure {
    pub subject: Vec<u8>,
    pub provenance: Provenance,
    pub time: u64,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: CommitId,
    pub parent: Option<CommitId>,
    /// For a merge, the head of the branch merged in.
    pub merged: Option<CommitId>,
    pub branch: String,
    pub provenance: Provenance,
    pub time: u64,
    /// The cells this commit wrote, sorted by key.
    written: Vec<Written>,
    /// The whole tree after this commit, kept for one commit in about
    /// `TREE_EVERY` along each line of history. The trees of the others
    /// are rebuilt from the nearest one before them.
    tree: Option<Tree>,
}

/// A cell a commit wrote, and the cell it replaced on its branch.
#[derive(Debug, Clone)]
struct Written {
    key: Bytes,
    cell: Bytes,
    replaced: Option<Bytes>,
}

/// How often along a line of history a commit keeps its whole tree.
/// Rebuilding the tree of any commit then replays at most this many
/// commits' cells, while memory grows with the cells written rather than
/// with a copied path of the tree per commit.
const TREE_EVERY: usize = 1024;

impl CommitInfo {
    fn written(&self, key: &[u8]) -> Option<&Written> {
        self.written
            .binary_search_by(|written| (*written.key).cmp(key))
            .ok()
            .map(|index| &self.written[index])
    }
}

#[derive(Debug)]
pub enum StoreError {
    Log(LogError),
    UnknownBranch(String),
    UnknownCommit(CommitId),
    /// The commit is not in the branch's history.
    NotOnBranch(CommitId),
    /// Names no branch or commit.
    UnknownName(String),
    /// The start of an id that more than one commit has.
    AmbiguousName(String),
    BranchExists(String),
    /// Empty, or longer than a label should be.
    BadBranchName,
    /// The log holds a record the store cannot apply, such as a commit to a
    /// branch that was never created. The log itself checked out.
    Inconsistent(String),
    /// The committer stopped before the commit was written.
    Stopped,
    Keys(KeyError),
    /// The log has values sealed under this many keys the key file does
    /// not hold. Nothing was written.
    MissingKeys(usize),
    /// Opened with a key file, the log holds a value that is not sealed.
    NotSealed,
    /// Erasing needs encrypted values; the store was opened without keys.
    NoKeys,
    /// A table name is one part of a key: not empty, without a `/`.
    BadTableName,
    /// The table has been written to, on some branch, so how it is held
    /// can no longer be declared.
    TableInUse(Vec<u8>),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(error) => write!(f, "{error}"),
            Self::UnknownBranch(name) => write!(f, "no branch named {name:?}"),
            Self::UnknownCommit(id) => write!(f, "no commit {}", short(id)),
            Self::NotOnBranch(id) => write!(f, "commit {} is not on this branch", short(id)),
            Self::UnknownName(name) => write!(f, "no branch or commit named {name:?}"),
            Self::AmbiguousName(name) => write!(f, "more than one commit starts with {name:?}"),
            Self::BranchExists(name) => write!(f, "a branch named {name:?} already exists"),
            Self::BadBranchName => write!(f, "a branch name must be 1 to {MAX_BRANCH_NAME} bytes"),
            Self::Inconsistent(what) => write!(f, "the log cannot be applied: {what}"),
            Self::Stopped => write!(f, "the committer has stopped; nothing was written"),
            Self::Keys(error) => write!(f, "{error}"),
            Self::MissingKeys(count) => write!(
                f,
                "the key file lacks {count} key(s) the log needs; nothing was written"
            ),
            Self::NotSealed => write!(f, "the log holds values written without encryption"),
            Self::NoKeys => write!(f, "values are not encrypted, so nothing can be erased"),
            Self::BadTableName => write!(f, "a table name must be one part of a key, without '/'"),
            Self::TableInUse(name) => write!(
                f,
                "table {:?} already has keys; its engine can only be declared before the first write",
                String::from_utf8_lossy(name)
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<KeyError> for StoreError {
    fn from(error: KeyError) -> Self {
        Self::Keys(error)
    }
}

impl From<LogError> for StoreError {
    fn from(error: LogError) -> Self {
        Self::Log(error)
    }
}

/// A branch's latest commit, or none yet, and the cells it holds.
#[derive(Debug, Clone)]
struct Head {
    commit: Option<CommitId>,
    state: Tree,
    /// Commits along this line of history since the last that kept its
    /// whole tree.
    since_tree: usize,
}

pub struct Store<S: Storage, C: Clock> {
    log: Log<S>,
    clock: C,
    branches: BTreeMap<String, Head>,
    commits: HashMap<CommitId, Arc<CommitInfo>>,
    /// The time of the latest commit, so times never go backwards.
    last_time: u64,
    /// Present when values are encrypted.
    sealer: Option<Sealer>,
    erasures: Vec<Erasure>,
    tables: Tables,
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Opens the log and replays every record. Values are kept as given.
    pub fn open(storage: S, clock: C) -> Result<Self, StoreError> {
        Self::open_with(storage, clock, None)
    }

    /// Opens the log with its key file, and encrypts every value from here
    /// on. Refuses a log whose values are not all sealed under keys the key
    /// file holds, before anything is written.
    pub fn open_sealed(
        storage: S,
        keys: Keys<Box<dyn KeyStorage + Send>>,
        clock: C,
    ) -> Result<Self, StoreError> {
        Self::open_with(storage, clock, Some(Sealer::new(keys)))
    }

    fn open_with(storage: S, clock: C, sealer: Option<Sealer>) -> Result<Self, StoreError> {
        let (log, records) = Log::open(storage)?;
        if let Some(sealer) = &sealer {
            check_sealed(&records, &sealer.ring())?;
        }
        let erased: Vec<Fingerprint> = records
            .iter()
            .filter_map(|record| match record {
                Record::Erase(erase) => Some(erase.key_fingerprint),
                _ => None,
            })
            .collect();
        let mut store = Self {
            log,
            clock,
            branches: BTreeMap::from([(
                MAIN_BRANCH.to_owned(),
                Head {
                    commit: None,
                    state: Tree::empty(),
                    since_tree: 0,
                },
            )]),
            commits: HashMap::new(),
            last_time: 0,
            sealer,
            erasures: Vec::new(),
            tables: Tables::default(),
        };
        for record in &records {
            store
                .apply(record)
                .map_err(|e| StoreError::Inconsistent(e.to_string()))?;
        }
        // An erasure is logged before its key is destroyed; finish any a
        // crash cut short. Only now, after the log checked out against the
        // key file, is the key file written.
        if let Some(sealer) = &mut store.sealer {
            for fingerprint in &erased {
                sealer.erase_fingerprint(fingerprint)?;
            }
        }
        Ok(store)
    }

    /// Writes `edits` to `branch` as one commit and returns its id once it
    /// is durable. If the log fails, nothing changes and, as with the log,
    /// the store refuses further writes.
    pub fn commit(
        &mut self,
        branch: &str,
        edits: Vec<Edit>,
        provenance: Provenance,
    ) -> Result<CommitId, StoreError> {
        let edits = self.seal(edits)?;
        self.commit_with(branch, edits, provenance, None)
    }

    /// Writes several commits with one sync and returns once all of them
    /// are durable: a crash keeps all or none. They apply in order, each a
    /// commit of its own, so two to one branch follow each other. A request
    /// to an unknown branch is refused on its own; the result for each
    /// request is in its place. If the log fails, none is written.
    pub fn commit_all(
        &mut self,
        requests: Vec<CommitRequest>,
    ) -> Result<Vec<Result<CommitId, StoreError>>, StoreError> {
        let time = self.next_time();
        let mut commits = Vec::with_capacity(requests.len());
        let mut refusals = Vec::with_capacity(requests.len());
        for mut request in requests {
            request.edits = self.seal(request.edits)?;
            refusals.push(self.require_branch(&request.branch).err());
            if refusals.last().is_some_and(Option::is_none) {
                commits.push(new_commit(request, time, None));
            }
        }
        let mut ids = self.write(commits)?.into_iter();
        Ok(refusals
            .into_iter()
            .map(|refusal| match refusal {
                Some(error) => Err(error),
                None => Ok(ids.next().expect("one id per accepted request")),
            })
            .collect())
    }

    fn commit_with(
        &mut self,
        branch: &str,
        edits: Vec<Edit>,
        provenance: Provenance,
        merged: Option<CommitId>,
    ) -> Result<CommitId, StoreError> {
        self.require_branch(branch)?;
        let request = CommitRequest {
            branch: branch.to_owned(),
            edits,
            provenance,
        };
        let commit = new_commit(request, self.next_time(), merged);
        let ids = self.write(vec![commit])?;
        Ok(ids[0])
    }

    /// Seals the values of edits coming in, if values are encrypted.
    /// Merges and undo copy values already sealed and do not come here.
    fn seal(&mut self, edits: Vec<Edit>) -> Result<Vec<Edit>, StoreError> {
        let Some(sealer) = &mut self.sealer else {
            return Ok(edits);
        };
        edits
            .into_iter()
            .map(|edit| {
                let value = match edit.value {
                    Some(value) => Some(sealer.seal(&edit.key, &value)?),
                    None => None,
                };
                Ok(Edit {
                    key: edit.key,
                    value,
                })
            })
            .collect()
    }

    /// Opens a value going out, if values are encrypted.
    fn reveal(&self, key: &[u8], value: Option<Bytes>) -> Option<Bytes> {
        reveal(self.sealer.as_ref().map(Sealer::ring).as_ref(), key, value)
    }

    /// Destroys the subject's key, so none of its values, current or past,
    /// on any branch, can be read again. The history keeps its shape: the
    /// commits, who made them and why, and which keys they changed. The
    /// erasure is logged first, with who did it and why, so a crash before
    /// the key is destroyed is finished when the store next opens. Returns
    /// whether the subject had a key.
    pub fn erase(&mut self, subject: &[u8], provenance: Provenance) -> Result<bool, StoreError> {
        let time = self.next_time();
        let sealer = self.sealer.as_mut().ok_or(StoreError::NoKeys)?;
        let Some(key_fingerprint) = sealer.fingerprint_of(subject) else {
            return Ok(false);
        };
        let record = Record::Erase(Erase {
            subject: subject.to_vec(),
            key_fingerprint,
            who: provenance.who,
            why: provenance.why,
            approved_by: provenance.approved_by,
            time,
        });
        self.log.append(&record)?;
        self.apply(&record)?;
        let sealer = self.sealer.as_mut().ok_or(StoreError::NoKeys)?;
        sealer.erase(subject, &key_fingerprint)?;
        Ok(true)
    }

    /// Declares that `table`, the first part of its keys, is held by
    /// `engine`. Logged with who did it and why. Refused once anything has
    /// been written to the table, on any branch, and for a name that is not
    /// one part of a key; nothing is written then.
    pub fn declare_table(
        &mut self,
        table: &[u8],
        engine: Engine,
        provenance: Provenance,
    ) -> Result<(), StoreError> {
        self.tables
            .check(table)
            .map_err(|refusal| refused(refusal, table))?;
        let time = self.next_time();
        let record = Record::Table(Table {
            name: table.to_vec(),
            engine,
            who: provenance.who,
            why: provenance.why,
            approved_by: provenance.approved_by,
            time,
        });
        self.log.append(&record)?;
        self.apply(&record).map(drop)
    }

    /// Every declared table and its engine. A table not here keeps its
    /// history.
    pub fn tables(&self) -> &BTreeMap<Vec<u8>, Engine> {
        self.tables.declared()
    }

    /// The engine that holds `key`'s table.
    pub fn engine_of(&self, key: &[u8]) -> Engine {
        self.tables.engine_of(key)
    }

    /// Every subject erased, oldest first.
    pub fn erasures(&self) -> &[Erasure] {
        &self.erasures
    }

    /// Never earlier than a time already recorded.
    fn next_time(&mut self) -> u64 {
        self.clock.now_micros().max(self.last_time)
    }

    /// Appends the commits as one record, then applies them in order.
    fn write(&mut self, mut commits: Vec<Commit>) -> Result<Vec<CommitId>, StoreError> {
        let record = match commits.len() {
            0 => return Ok(Vec::new()),
            1 => Record::Commit(commits.pop().expect("one commit")),
            _ => Record::Group(commits),
        };
        self.log.append(&record)?;
        self.apply(&record)
    }

    /// Makes a branch that starts where `from` is now.
    pub fn create_branch(&mut self, name: &str, from: &str) -> Result<(), StoreError> {
        if name.is_empty() || name.len() > MAX_BRANCH_NAME {
            return Err(StoreError::BadBranchName);
        }
        if self.branches.contains_key(name) {
            return Err(StoreError::BranchExists(name.to_owned()));
        }
        self.require_branch(from)?;
        let record = Record::Branch(Branch {
            name: name.to_owned(),
            from: from.to_owned(),
        });
        self.log.append(&record)?;
        self.apply(&record).map(drop)
    }

    /// The live value of `key` on `branch`.
    pub fn get(&self, branch: &str, key: &[u8]) -> Option<Bytes> {
        self.cell(branch, key)?.value
    }

    /// Every branch as it is now, to read from other threads.
    pub fn view(&self) -> View {
        View::new(
            self.branches
                .iter()
                .map(|(name, head)| (name.clone(), head.state.clone()))
                .collect(),
            self.sealer.as_ref().map(Sealer::ring),
        )
    }

    /// The value of `key` as `commit` left it.
    pub fn get_at(&self, commit: &CommitId, key: &[u8]) -> Option<Bytes> {
        let value = decode_cell(&self.cell_at(commit, key)?).value;
        self.reveal(key, value)
    }

    /// The cell of `key` on `branch`: its value, or the removal, and the
    /// commit that made it. `None` if the key was never written there.
    pub fn cell(&self, branch: &str, key: &[u8]) -> Option<Cell> {
        self.raw_cell(branch, key)
            .map(|cell| self.revealed(key, cell))
    }

    fn raw_cell(&self, branch: &str, key: &[u8]) -> Option<Cell> {
        self.branches
            .get(branch)?
            .state
            .get(key)
            .map(|bytes| decode_cell(bytes))
    }

    fn revealed(&self, key: &[u8], cell: Cell) -> Cell {
        let was_written = cell.value.is_some();
        let value = self.reveal(key, cell.value);
        Cell {
            erased: was_written && value.is_none(),
            value,
            changed_by: cell.changed_by,
        }
    }

    /// Every change to `key` that `branch` can see, newest first.
    pub fn history(&self, branch: &str, key: &[u8]) -> Vec<Cell> {
        self.history_up_to(branch, key, usize::MAX)
    }

    /// Every live key and value on `branch`, in key order.
    pub fn entries(&self, branch: &str) -> Vec<(Bytes, Bytes)> {
        let Some(head) = self.branches.get(branch) else {
            return Vec::new();
        };
        self.live_entries(&head.state)
    }

    /// Every live key and value as `commit` left it, in key order.
    pub fn entries_at(&self, commit: &CommitId) -> Vec<(Bytes, Bytes)> {
        self.tree_at(commit)
            .map(|tree| self.live_entries(&tree))
            .unwrap_or_default()
    }

    /// The cell of `key` as `commit` left it: written by the commit itself
    /// or one before it, found by walking back no further than the nearest
    /// commit that keeps its whole tree.
    fn cell_at(&self, commit: &CommitId, key: &[u8]) -> Option<Bytes> {
        let mut next = Some(*commit);
        while let Some(id) = next {
            let info = self.commits.get(&id)?;
            if let Some(written) = info.written(key) {
                return Some(written.cell.clone());
            }
            if let Some(tree) = &info.tree {
                return tree.get(key).cloned();
            }
            next = info.parent;
        }
        None
    }

    /// The whole tree as `commit` left it: a branch's head as it is, any
    /// other rebuilt from the nearest tree kept before it, or from the empty
    /// tree, by replaying the cells written since.
    fn tree_at(&self, commit: &CommitId) -> Option<Tree> {
        if let Some(head) = self.branches.values().find(|h| h.commit == Some(*commit)) {
            return Some(head.state.clone());
        }
        // Walking back from the newest, the first cell seen for a key is the
        // one that stands, so the replay is one insert of each key once.
        let mut latest: BTreeMap<Bytes, Bytes> = BTreeMap::new();
        let mut next = Some(*commit);
        let base = loop {
            let Some(id) = next else {
                break Tree::empty();
            };
            let info = self.commits.get(&id)?;
            if let Some(tree) = &info.tree {
                break tree.clone();
            }
            for written in &info.written {
                latest
                    .entry(written.key.clone())
                    .or_insert_with(|| written.cell.clone());
            }
            next = info.parent;
        };
        Some(base.insert_sorted(latest.into_iter().collect()))
    }

    /// The latest commit on `branch`, or `None` if it has none or does not
    /// exist.
    pub fn head(&self, branch: &str) -> Option<CommitId> {
        self.branches.get(branch)?.commit
    }

    pub fn branches(&self) -> impl Iterator<Item = &str> {
        self.branches.keys().map(String::as_str)
    }

    /// The commits of `branch`, newest first, following first parents: the
    /// branch's own line, with each merge as one commit.
    pub fn log(&self, branch: &str) -> Vec<&CommitInfo> {
        self.log_up_to(branch, usize::MAX)
    }

    /// Finds a commit by name: a branch (its latest commit), a branch and
    /// `~n` for the commit `n` steps back on its own line, or the start of a
    /// commit id in hex, at least `MIN_ID_PREFIX` characters and matching
    /// one commit only.
    pub fn resolve(&self, name: &str) -> Result<CommitId, StoreError> {
        let (branch, back) = match name.rsplit_once('~') {
            Some((branch, n)) => match n.parse::<usize>() {
                Ok(n) => (branch, n),
                Err(_) => return Err(StoreError::UnknownName(name.to_owned())),
            },
            None => (name, 0),
        };
        if self.branches.contains_key(branch) {
            return self
                .log(branch)
                .get(back)
                .map(|info| info.id)
                .ok_or_else(|| StoreError::UnknownName(name.to_owned()));
        }
        let prefix = name.to_ascii_lowercase();
        let is_hex = prefix.len() >= MIN_ID_PREFIX && prefix.bytes().all(|b| b.is_ascii_hexdigit());
        if !is_hex {
            return Err(StoreError::UnknownName(name.to_owned()));
        }
        let mut found = self
            .commits
            .keys()
            .filter(|id| hex(id).starts_with(&prefix));
        match (found.next(), found.next()) {
            (Some(id), None) => Ok(*id),
            (Some(_), Some(_)) => Err(StoreError::AmbiguousName(name.to_owned())),
            (None, _) => Err(StoreError::UnknownName(name.to_owned())),
        }
    }

    pub fn commit_info(&self, id: &CommitId) -> Option<&CommitInfo> {
        self.commits.get(id).map(Arc::as_ref)
    }

    /// The time recorded on the latest commit.
    pub fn last_time(&self) -> u64 {
        self.last_time
    }

    /// How many commits keep a whole tree. Tests use this to check that
    /// memory grows with the edits, not with a tree per commit.
    #[doc(hidden)]
    pub fn trees_kept(&self) -> usize {
        self.commits
            .values()
            .filter(|info| info.tree.is_some())
            .count()
    }

    fn require_branch(&self, name: &str) -> Result<(), StoreError> {
        if self.branches.contains_key(name) {
            Ok(())
        } else {
            Err(StoreError::UnknownBranch(name.to_owned()))
        }
    }

    /// Applies a record and returns the ids of the commits it held.
    fn apply(&mut self, record: &Record) -> Result<Vec<CommitId>, StoreError> {
        match record {
            Record::Branch(branch) => {
                let from = self
                    .branches
                    .get(&branch.from)
                    .ok_or_else(|| StoreError::UnknownBranch(branch.from.clone()))?
                    .clone();
                if self.branches.insert(branch.name.clone(), from).is_some() {
                    return Err(StoreError::BranchExists(branch.name.clone()));
                }
                Ok(Vec::new())
            }
            Record::Commit(commit) => Ok(vec![self.apply_commit(commit)?]),
            Record::Erase(erase) => {
                self.erasures.push(Erasure {
                    subject: erase.subject.clone(),
                    provenance: Provenance {
                        who: erase.who.clone(),
                        why: erase.why.clone(),
                        approved_by: erase.approved_by.clone(),
                    },
                    time: erase.time,
                });
                self.last_time = self.last_time.max(erase.time);
                Ok(Vec::new())
            }
            Record::Group(commits) => commits
                .iter()
                .map(|commit| self.apply_commit(commit))
                .collect(),
            Record::Table(table) => {
                self.tables
                    .declare(&table.name, table.engine)
                    .map_err(|refusal| refused(refusal, &table.name))?;
                self.last_time = self.last_time.max(table.time);
                Ok(Vec::new())
            }
        }
    }

    fn apply_commit(&mut self, commit: &Commit) -> Result<CommitId, StoreError> {
        let head = self
            .branches
            .get_mut(&commit.branch)
            .ok_or_else(|| StoreError::UnknownBranch(commit.branch.clone()))?;
        let id = commit_id(head.commit, commit);
        let cells = cells_of(&head.state, commit, &id);
        for (key, _) in &cells {
            self.tables.note_written(key);
        }
        let written = cells
            .iter()
            .map(|(key, cell)| Written {
                key: key.clone(),
                cell: cell.clone(),
                replaced: head.state.get(key).cloned(),
            })
            .collect();
        let state = head.state.insert_sorted(cells);
        let since_tree = (head.since_tree + 1) % TREE_EVERY;
        let info = CommitInfo {
            id,
            parent: head.commit,
            merged: commit.merged,
            branch: commit.branch.clone(),
            provenance: Provenance {
                who: commit.who.clone(),
                why: commit.why.clone(),
                approved_by: commit.approved_by.clone(),
            },
            time: commit.time,
            written,
            tree: (since_tree == 0).then(|| state.clone()),
        };
        *head = Head {
            commit: Some(id),
            state,
            since_tree,
        };
        self.commits.insert(id, Arc::new(info));
        self.last_time = self.last_time.max(commit.time);
        Ok(id)
    }

    fn live_entries(&self, state: &Tree) -> Vec<(Bytes, Bytes)> {
        state
            .iter()
            .filter_map(|(key, bytes)| {
                let value = self.reveal(key, decode_cell(bytes).value)?;
                Some((key.clone(), value))
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn storage_mut(&mut self) -> &mut S {
        self.log.storage_mut()
    }

    #[cfg(test)]
    pub(crate) fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }
}

const REMOVED: u8 = 0;
const PRESENT: u8 = 1;
const COMMIT_ID_LEN: usize = 32;

fn new_commit(request: CommitRequest, time: u64, merged: Option<CommitId>) -> Commit {
    Commit {
        branch: request.branch,
        changes: request.edits,
        who: request.provenance.who,
        why: request.provenance.why,
        approved_by: request.provenance.approved_by,
        time,
        merged,
    }
}

fn refused(refusal: Refusal, table: &[u8]) -> StoreError {
    match refusal {
        Refusal::BadName => StoreError::BadTableName,
        Refusal::InUse => StoreError::TableInUse(table.to_vec()),
    }
}

/// The cells `commit` writes, sorted by key. Edits apply in order, so the
/// last edit of a key wins, and removing what is not live at that point
/// changes nothing and records nothing.
fn cells_of(state: &Tree, commit: &Commit, id: &CommitId) -> Vec<(Bytes, Bytes)> {
    let mut written: BTreeMap<&[u8], Option<&[u8]>> = BTreeMap::new();
    for edit in &commit.changes {
        let is_live = match written.get(edit.key.as_slice()) {
            Some(value) => value.is_some(),
            None => state
                .get(&edit.key)
                .is_some_and(|bytes| decode_cell(bytes).value.is_some()),
        };
        if edit.value.is_none() && !is_live {
            continue;
        }
        written.insert(&edit.key, edit.value.as_deref());
    }
    written
        .into_iter()
        .map(|(key, value)| (Arc::from(key), encode_cell(id, value)))
        .collect()
}

/// A cell in the tree: the commit id, then a flag, then the value if any.
fn encode_cell(commit: &CommitId, value: Option<&[u8]>) -> Bytes {
    let mut bytes = Vec::with_capacity(COMMIT_ID_LEN + 1 + value.map_or(0, <[u8]>::len));
    bytes.extend_from_slice(commit);
    match value {
        None => bytes.push(REMOVED),
        Some(value) => {
            bytes.push(PRESENT);
            bytes.extend_from_slice(value);
        }
    }
    Arc::from(bytes)
}

/// Cells are only written by `encode_cell`, so their layout is known.
fn decode_cell(bytes: &[u8]) -> Cell {
    let (id, rest) = bytes.split_at(COMMIT_ID_LEN);
    let changed_by: CommitId = id.try_into().expect("a cell starts with a commit id");
    let value = match rest.split_first() {
        Some((&PRESENT, value)) => Some(Arc::from(value)),
        _ => None,
    };
    Cell {
        value,
        changed_by,
        erased: false,
    }
}

/// Every field that makes the commit what it is, each length-prefixed, so
/// no two different commits hash alike.
fn commit_id(parent: Option<CommitId>, commit: &Commit) -> CommitId {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(b"umbrachyx commit");
    field(parent.as_ref().map_or(&[][..], |id| &id[..]));
    field(commit.branch.as_bytes());
    field(commit.who.as_bytes());
    field(commit.why.as_bytes());
    field(&[u8::from(commit.approved_by.is_some())]);
    field(commit.approved_by.as_deref().unwrap_or("").as_bytes());
    field(&commit.time.to_le_bytes());
    field(commit.merged.as_ref().map_or(&[][..], |id| &id[..]));
    for edit in &commit.changes {
        field(&edit.key);
        field(&[u8::from(edit.value.is_some())]);
        field(edit.value.as_deref().unwrap_or_default());
    }
    *hasher.finalize().as_bytes()
}

/// The first bytes of a commit id, in hex: enough to name it in practice.
pub fn short(id: &CommitId) -> String {
    hex(id)[..12].to_owned()
}

fn hex(id: &CommitId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// Opens `value` for `key` with the keys in `ring`, or passes it through
/// when values are not encrypted. A value whose key is gone reads as
/// absent.
fn reveal(ring: Option<&Ring>, key: &[u8], value: Option<Bytes>) -> Option<Bytes> {
    match ring {
        None => value,
        Some(ring) => seal::open(ring, key, &value?),
    }
}

/// Every value in the log must be sealed under a key the key file holds.
/// Checked before anything is written, since a write could cut a key file
/// that only looks torn.
fn check_sealed(records: &[Record], ring: &Ring) -> Result<(), StoreError> {
    let ring = ring
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut missing = HashSet::new();
    let erased: HashSet<Fingerprint> = records
        .iter()
        .filter_map(|record| match record {
            Record::Erase(erase) => Some(erase.key_fingerprint),
            _ => None,
        })
        .collect();
    let commits = records.iter().flat_map(|record| match record {
        Record::Branch(_) | Record::Erase(_) | Record::Table(_) => &[][..],
        Record::Commit(commit) => std::slice::from_ref(commit),
        Record::Group(commits) => commits.as_slice(),
    });
    for value in commits
        .flat_map(|c| &c.changes)
        .filter_map(|e| e.value.as_ref())
    {
        let fingerprint = seal::key_fingerprint(value).ok_or(StoreError::NotSealed)?;
        if !ring.contains_key(&fingerprint) && !erased.contains(&fingerprint) {
            missing.insert(fingerprint);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(StoreError::MissingKeys(missing.len()))
    }
}
