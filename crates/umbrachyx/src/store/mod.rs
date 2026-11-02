//! The store: the log is the truth, and commits, branches and their trees
//! are rebuilt from it.
//!
//! A commit is acknowledged once its record is synced to the log; only then
//! is it applied. Opening the store replays the log, so nothing the store
//! holds can be missing from the log.
//!
//! Every commit keeps the tree it produced. Versions share every node they
//! have in common, so keeping them all costs only the paths each commit
//! rewrote. A branch is a name for a commit, and making one copies nothing.
//!
//! Each value in the tree is a cell: the value, or a mark that it was
//! removed, and the commit that last changed it. Following that commit to
//! its parent and reading the key there gives the change before, so the
//! history of a key costs the number of times it changed, not the number of
//! commits.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use crate::log::{Branch, Commit, Edit, Log, LogError, Record, Storage};
use crate::tree::{Bytes, Tree};

mod merge;

pub use merge::{Conflict, Merged, Reverted, ValueChange};

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
    /// `None` when the change was a removal.
    pub value: Option<Bytes>,
    pub changed_by: CommitId,
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
    /// The cells after this commit.
    state: Tree,
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
        }
    }
}

impl std::error::Error for StoreError {}

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
}

pub struct Store<S: Storage, C: Clock> {
    log: Log<S>,
    clock: C,
    branches: BTreeMap<String, Head>,
    commits: HashMap<CommitId, Arc<CommitInfo>>,
    /// The time of the latest commit, so times never go backwards.
    last_time: u64,
}

impl<S: Storage, C: Clock> Store<S, C> {
    /// Opens the log and replays every record.
    pub fn open(storage: S, clock: C) -> Result<Self, StoreError> {
        let (log, records) = Log::open(storage)?;
        let mut store = Self {
            log,
            clock,
            branches: BTreeMap::from([(
                MAIN_BRANCH.to_owned(),
                Head {
                    commit: None,
                    state: Tree::empty(),
                },
            )]),
            commits: HashMap::new(),
            last_time: 0,
        };
        for record in &records {
            store
                .apply(record)
                .map_err(|e| StoreError::Inconsistent(e.to_string()))?;
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
        for request in requests {
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

    /// The value of `key` as `commit` left it.
    pub fn get_at(&self, commit: &CommitId, key: &[u8]) -> Option<Bytes> {
        decode_cell(self.commits.get(commit)?.state.get(key)?).value
    }

    /// The cell of `key` on `branch`: its value, or the removal, and the
    /// commit that made it. `None` if the key was never written there.
    pub fn cell(&self, branch: &str, key: &[u8]) -> Option<Cell> {
        self.branches
            .get(branch)?
            .state
            .get(key)
            .map(|bytes| decode_cell(bytes))
    }

    /// Every change to `key` that `branch` can see, newest first.
    pub fn history(&self, branch: &str, key: &[u8]) -> Vec<Cell> {
        let mut history = Vec::new();
        let mut cell = self.cell(branch, key);
        while let Some(current) = cell {
            let parent = self
                .commits
                .get(&current.changed_by)
                .and_then(|commit| commit.parent)
                .and_then(|parent| self.commits.get(&parent));
            cell = parent
                .and_then(|parent| parent.state.get(key))
                .map(|bytes| decode_cell(bytes));
            history.push(current);
        }
        history
    }

    /// Every live key and value on `branch`, in key order.
    pub fn entries(&self, branch: &str) -> Vec<(Bytes, Bytes)> {
        let Some(head) = self.branches.get(branch) else {
            return Vec::new();
        };
        live_entries(&head.state)
    }

    /// Every live key and value as `commit` left it, in key order.
    pub fn entries_at(&self, commit: &CommitId) -> Vec<(Bytes, Bytes)> {
        self.commits
            .get(commit)
            .map(|info| live_entries(&info.state))
            .unwrap_or_default()
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
        let mut commits = Vec::new();
        let mut next = self.head(branch);
        while let Some(info) = next.and_then(|id| self.commits.get(&id)) {
            commits.push(info.as_ref());
            next = info.parent;
        }
        commits
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
            Record::Group(commits) => commits
                .iter()
                .map(|commit| self.apply_commit(commit))
                .collect(),
        }
    }

    fn apply_commit(&mut self, commit: &Commit) -> Result<CommitId, StoreError> {
        let head = self
            .branches
            .get_mut(&commit.branch)
            .ok_or_else(|| StoreError::UnknownBranch(commit.branch.clone()))?;
        let id = commit_id(head.commit, commit);
        let state = head.state.insert_sorted(cells_of(&head.state, commit, &id));
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
            state: state.clone(),
        };
        *head = Head {
            commit: Some(id),
            state,
        };
        self.commits.insert(id, Arc::new(info));
        self.last_time = self.last_time.max(commit.time);
        Ok(id)
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
    Cell { value, changed_by }
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

fn live_entries(state: &Tree) -> Vec<(Bytes, Bytes)> {
    state
        .iter()
        .filter_map(|(key, bytes)| Some((key.clone(), decode_cell(bytes).value?)))
        .collect()
}
