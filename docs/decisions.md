# Decisions

Technical choices that shape the code, and why they were made. A technology
without an entry here is not adopted.

## Dependencies

| Crate | Used for | Why this one |
|---|---|---|
| `crc32fast` | Checksum on every log record, so a torn or damaged record is detected on recovery | Widely used and maintained, MIT OR Apache-2.0, no dependencies beyond `cfg-if`. Hardware-accelerated where available, so the check costs little on the write path. A checksum is too small a thing to justify writing and verifying our own |
| `rustix` (dev, Unix only) | A plain `fsync` in the sync latency example, to compare it with `F_FULLFSYNC` | std has no way to call a plain `fsync` on macOS. Maintained by the Bytecode Alliance, Apache-2.0 / MIT, and safe to call, where `libc` would need `unsafe`. Not linked into the library |
| `blake3` | The hash that names every node of the tree, so equal hashes mean equal content and a diff can skip shared subtrees | Collision resistance is required: a diff or a sync between replicas trusts that equal hashes mean equal data. CC0 / Apache-2.0, maintained by its authors. |
| `biscuit-auth` | Capability tokens: what a caller may read or write, carried by the caller | Tokens can be narrowed offline by whoever holds them and never widened, and are verified with a public key, so nothing secret sits on the side that checks them. Writing an authorization scheme ourselves would need its own security review. Apache-2.0, maintained under the Eclipse Foundation. Default features are off; `datalog-macro` stays on because 6.0.0 does not build without it |
| `serde_json` | Reading and writing the JSON-RPC messages of the MCP session | The de facto JSON crate for Rust, MIT OR Apache-2.0, maintained by the serde authors. It was already built as a dependency of `biscuit-auth`, so it adds no crate. Used through `Value` only, without derive macros. The MCP protocol itself is written here rather than taken from the official Rust SDK, which runs on an async runtime the server does not use; the part served is four methods |
| `getrandom` | The random bytes of each subject's encryption key | The operating system's random source, nothing else: the smallest way to get key material. MIT OR Apache-2.0, maintained by the rust-random project, and already built as a dependency of `biscuit-auth` at the same version, so it adds no crate |
| `aes-siv` | Encrypting each value under its subject's key, so that destroying the key erases the subject | Deterministic authenticated encryption (RFC 5297) from RustCrypto, Apache-2.0 OR MIT, maintained with the rest of its AEADs. Deterministic is what the store needs: equal values must stay equal once encrypted for the tree, diffs and merges to work unchanged, and AES-SIV is built to be used that way rather than with a nonce we would have to invent. Default features are off; the random source is `getrandom`, already a dependency. It brings its RustCrypto building blocks (`aes`, `cmac`, `ctr` and their traits) |

## The log

- **A write is acknowledged only after its record is synced.** The log is the
  source of truth; the tree and the history are rebuilt from it.
- **Syncing uses `File::sync_data`**, which is `fdatasync` on Linux,
  `fcntl(F_FULLFSYNC)` on macOS and `FlushFileBuffers` on Windows. A plain
  `fsync` on macOS does not reach the disk.
- **Durability is tested on Linux, macOS and Windows.** Most machines this is
  meant to run on are Windows, so it is a target in CI, not inferred from
  Linux. On Windows the directory entry of a new log file is not flushed
  separately, because a directory cannot be opened as a file there; NTFS
  journals it as metadata.
- **A failed sync stops the log from accepting writes.** After a failed sync
  the state of unsynced bytes is unknown, and retrying could acknowledge data
  that was lost.
- **A damaged record at the end of the log is cut off; damage anywhere else
  stops the log from opening.** Appends are synchronous, so only the end can
  be torn by a crash. When a record does not check out, recovery looks for a
  whole, checked record anywhere after it: if one exists, acknowledged data
  was damaged and the log refuses to open rather than dropping it. Damage
  inside the very last record cannot be told apart from a torn write.
- **A record is a commit, a new branch, or a group of commits.** A commit
  holds the branch, the keys it sets or removes, who made it, why, who
  approved it and when. It is one frame, so a commit that touches many keys
  is in the log whole or not at all. A new branch names the branch it
  starts from. The payload starts with a format version; a frame whose
  checksum holds but which does not decode, whatever its version, stops the
  log from opening rather than being cut as a torn write, because a torn
  write cannot pass the checksum.
- **Commits acknowledged by one sync share one frame (group commit).**
  Several commits written together are one group record, applied in
  order, so a crash keeps all of them or none. Writing them as separate
  frames with one sync would break the rule above: a power cut could tear
  one frame in the middle and keep the frames after it, and recovery would
  read that as damage to acknowledged data and refuse to open. The
  deterministic simulation finds this at its first seed when the group is
  split into frames. A single commit is still written as a commit record.
- **Opening the log always syncs before returning records.** A process that
  died between writing and syncing leaves bytes the kernel still serves but
  the disk may not hold.
- **The log file is locked by one writer at a time**, so two processes cannot
  interleave records.

### What a durable write costs

Measured with `cargo run --release --example sync_latency` (2,000 writes of
128 bytes per method, one sync per write). These are indicative numbers from
one machine, recorded to compare sync methods, not to state the speed of the
database.

Apple M4 MacBook Air (fanless, 32 GB), macOS 26.6.2, internal SSD with APFS,
on AC power, 2026-11-02:

| Method | p50 (µs) | p99 (µs) | max (µs) |
|---|---|---|---|
| `File::sync_data` (`F_FULLFSYNC`) | 4,015 | 5,387 | 10,667 |
| `File::sync_all` (`F_FULLFSYNC`) | 4,011 | 5,139 | 10,745 |
| plain `fsync(2)` | 25 | 47 | 79 |
| `Log::append` (framing, checksum, `F_FULLFSYNC`) | 4,011 | 5,275 | 10,841 |

GitHub Actions runners, CI run 41407766219, 2026-11-02. The disks are
virtual and shared, so these are reference values only:

| Runner | Method | p50 (µs) | p99 (µs) | max (µs) |
|---|---|---|---|---|
| `ubuntu-latest` | `File::sync_data` (`fdatasync`) | 173 | 410 | 2,352 |
| `ubuntu-latest` | `File::sync_all` (`fsync`) | 180 | 844 | 5,235 |
| `ubuntu-latest` | `Log::append` | 177 | 419 | 6,160 |
| `windows-latest` | `File::sync_data` (`FlushFileBuffers`) | 3,796 | 8,897 | 31,092 |
| `windows-latest` | `File::sync_all` (`FlushFileBuffers`) | 3,916 | 9,107 | 30,876 |
| `windows-latest` | `Log::append` | 3,999 | 10,881 | 133,301 |

A plain `fsync` is about 160 times faster on the MacBook Air because it stops at
the drive's cache; it is not durable across a power cut and is not used. The
log adds nothing measurable on top of the sync itself, so the cost of a
durable write on macOS is the cost of `F_FULLFSYNC`, about 4 ms. Reaching
memory speed for writes will need group commit: one sync for many writes.
- **All disk access goes through one storage boundary**, so deterministic
  simulation tests can replace the disk with one that loses unsynced bytes,
  tears writes, and fails syncs.

## Deterministic simulation

- **The simulator is our own, not madsim or turmoil.** Both replace an
  async runtime and the network; the log is synchronous and has no network,
  so there is nothing for them to replace yet. This follows FoundationDB and
  TigerBeetle: one seeded random source, and a simulated disk behind the same
  storage boundary the real file uses. They will be looked at again when
  replication brings a network.
- **The random source is SplitMix64, written out in the code.** A failing
  seed has to replay the same run for as long as it sits in a bug report; a
  dependency upgrade that changed the stream would break that silently. A
  test pins its first outputs.
- **The simulated disk keeps what is durable apart from what the process
  sees.** Appends and truncates change only what is seen until a sync. A
  process crash keeps what is seen, as the kernel would; a power cut keeps
  what is durable, plus an unfinished write cut at any length with each
  512-byte sector of it kept, zeroed or filled with garbage independently.
  Syncs can fail, dropping the unsynced bytes or leaving them in flight, and
  a bit can flip in data that was already durable.
- **Each seed runs 64 steps** of writes, failed syncs, process crashes,
  power cuts and bit flips. After every restart it checks that every
  acknowledged record came back unchanged; that nothing else did except the
  one record whose sync was interrupted; that damage before the last record
  stops the log from opening, at the right offset; that what open returned
  survives a second power cut; that a log whose sync failed refuses further
  writes; and that the log's length matches its records. CI runs 10,000
  seeds.
- **The harness is checked against deliberately broken logs**: skipping the
  sync in open, not refusing writes after a failed sync, miscounting the
  length, treating interior damage as a torn tail, and ignoring a failed
  sync are each caught within the first seeds.
- **The clock is a boundary like the disk.** The store reads the time
  through a `Clock` trait. The simulated clock mostly moves forward a little
  and sometimes jumps back by up to a second, as a wall clock corrected by
  NTP can. The store never records a time earlier than one it already
  recorded, across commits and restarts.
- **The store has its own run**, on top of the log's: commits of up to six
  keys, including removes, over a small key space so they overwrite each
  other, with failed syncs, process crashes and power cuts. After every
  restart every branch must exist and hold exactly what the acknowledged
  commits wrote to it, an interrupted commit must be there whole or not at
  all, and recorded times must not go back. Branches are made from any
  branch, commits go to any of them, branches are merged into each other,
  and a branch's latest commit is undone, which must give back what the
  branch held before it. CI runs 10,000 seeds of each, in release mode; the
  results are the same as in debug mode.
- **The encrypted store has a run of its own**, with a simulated key file
  next to the simulated log: commits to the keys of a few subjects,
  erasures, appends to the key file torn part way, rewrites of it that
  fail with the old or the new file left, erasures whose log sync is
  interrupted, process crashes and power cuts. After every step the store
  must read every acknowledged value, unless its subject was erased since;
  read every value of an erased subject as erased, in its cells and its
  history, on every branch; and open again after every restart. An
  erasure whose sync was interrupted must be there whole or not at all.
  Its first seeds found two faults in the key file, both fixed (see
  [Erasing a subject](#erasing-a-subject)). The keys come from the
  operating system's random source, so the ciphertext differs between
  runs of a seed; nothing the run decides depends on it. A torn append
  leaves a prefix of its entry, or as many bytes of garbage.

## The tree

**The tree is a prolly tree**, as in Dolt and Noms: a B+ tree in which an
entry ends its node when the hash of its key, salted by the level, falls in
one bucket out of 32. Three candidates were written in
`crates/tree-candidates`, each a persistent map with BLAKE3 Merkle hashes,
and measured with the same synthetic data (a game inventory, 24-byte keys
and 32-byte values) by `cargo run --release -p umbrachyx-tree-candidates
--example tree_bench`.

- **Prolly tree.** The shape depends only on which keys are present.
- **B+ tree with Merkle hashes**, path copying, split in half when full. The
  shape depends on the order of inserts.
- **Hash array mapped trie (HAMT) with Merkle hashes**, 16 ways per level,
  as in the Jellyfish Merkle tree. The shape depends only on which keys are
  present, but keys are kept in hash order, not key order.

The `prollytree` crate was looked at and not used: its diff flattens every
child that differs instead of descending into it, so the cost does not stay
with the size of the change, and its default features pull in a git
implementation, an async runtime and a SQL engine.

### Measurements

Apple M4 MacBook Air (fanless, 32 GB), macOS 26.6.2, on AC power,
2026-11-02. A diff is timed against two trees holding the same final data:
"derived" was made from the base by inserts and shares its nodes; "rebuilt"
was built separately, as a replica or a branch built elsewhere would be.
Nodes are those the diff read on both sides; node sizes differ between
structures, so compare node counts down a column, not across rows of
different structures. Each lookup and insert is timed alone, and the clock
ticks about every 41 ns on this machine, so sub-microsecond lookups are
coarse.

| Structure | Records | Heap per record (bytes) | get p50 / p99 (µs) | insert p50 / p99 (µs) | Branch (ns) | Diff of 10 changes, derived: µs (nodes) | Diff of 10 changes, rebuilt: µs (nodes) |
|---|---|---|---|---|---|---|---|
| Prolly tree | 100,000 | 137 | 0.46 / 1.04 | 12.9 / 27.5 | 3 | 6 (46) | 5 (46) |
| Prolly tree | 1,000,000 | 137 | 0.88 / 1.46 | 14.3 / 30.7 | 3 | 7 (61) | 7 (61) |
| Prolly tree | 10,000,000 | 137 | 1.38 / 2.12 | 18.7 / 36.9 | 3 | 10 (70) | 10 (70) |
| B+ tree | 100,000 | 122 | 0.33 / 0.67 | 8.0 / 10.4 | 3 | 5 (40) | 389 (3,276) |
| B+ tree | 1,000,000 | 122 | 0.83 / 1.29 | 8.8 / 12.0 | 3 | 7 (52) | 5,055 (42,370) |
| B+ tree | 10,000,000 | 122 | 1.33 / 2.00 | 12.2 / 15.4 | 3 | 8 (66) | 51,224 (422,310) |
| HAMT | 100,000 | 149 | 0.12 / 0.29 | 5.1 / 8.7 | 3 | 2 (58) | 2 (58) |
| HAMT | 1,000,000 | 152 | 0.38 / 0.79 | 5.7 / 8.5 | 3 | 3 (80) | 3 (80) |
| HAMT | 10,000,000 | 155 | 0.71 / 2.00 | 6.3 / 9.0 | 3 | 3 (98) | 3 (98) |

The same 1,000,000 records with more changes:

| Structure | Changes | Diff, derived: µs (nodes) | Diff, rebuilt: µs (nodes) |
|---|---|---|---|
| Prolly tree | 10 | 8 (64) | 8 (64) |
| Prolly tree | 100 | 63 (421) | 64 (421) |
| Prolly tree | 1,000 | 585 (2,992) | 579 (2,992) |
| Prolly tree | 10,000 | 3,550 (17,193) | 3,497 (17,193) |
| B+ tree | 10 | 7 (58) | 4,899 (40,748) |
| B+ tree | 1,000 | 480 (2,760) | 5,109 (41,422) |
| HAMT | 10 | 3 (80) | 3 (80) |
| HAMT | 1,000 | 483 (4,290) | 391 (4,290) |

### Why the prolly tree

- **A diff costs what the change costs, between any two trees.** With 10
  changes, going from 100,000 to 10,000,000 records (100 times more) adds
  only one level: 46 to 70 nodes read. Going from 10 to 1,000 changes reads
  about 50 times more. This holds for the rebuilt tree too, because two
  trees with the same keys have the same shape. That is what replicas
  comparing root hashes (anti-entropy) and branches merged after being built
  apart need.
- **Keys stay in order.** Ranges such as "every slot of one player" are one
  contiguous walk, which the SQL layer planned on top of the key/value core
  will rely on.
- **Branching is one pointer copy**, about 3 ns, for every candidate.

### Why not the others

- **B+ tree:** a diff between two trees that were not derived from each
  other reads everything. At 10,000,000 records, 10 changes took 51 ms and
  422,310 nodes, a thousand times more than the derived case and growing
  with the data. A replica or a rebuilt branch would pay that on every
  comparison. Its faster inserts do not make up for it.
- **HAMT:** it is the fastest on every row and its diff scales as well as
  the prolly tree's. It was not chosen because keys are scattered by their
  hash: a range of keys would need a second, ordered index, and that index
  would need its own history. If ranges turn out not to matter, this is the
  decision to revisit.

### Costs taken on

- **Inserts are the slowest of the three**, 37 µs at p99 with 10,000,000
  records, because a node is re-hashed and re-cut whole on every change.
  The tree is updated after the log has made a write durable, not on the
  path that acknowledges it, and a durable write already costs about 4 ms
  on this machine.
- **Node size is not bounded.** A run of keys with no boundary is
  geometric in length, so nodes of a few hundred entries are rare but
  possible. A hard cap can be added if they show up.
- **The key hash that places boundaries is not keyed**, so keys chosen to
  collide could make nodes large. The database will need a keyed hash
  before it takes keys from untrusted clients.
- **Removing a key that ends a node merges that node with the next one.**
  Every node but the last on its level ends on a key that ends a node on
  its own level and on every level below; removing that key leaves the node
  open, and it is joined to its right neighbour along the seam, level by
  level, then cut again. The tree in the database (`crates/umbrachyx/src/tree`)
  has this; the candidates crate, kept as the record of the comparison, does
  not.

## Capability tokens

**Tokens are biscuits**, checked at the database's boundary before a
request reaches the log or the tree (`crates/umbrachyx/src/capability.rs`,
a prototype).

- **What a token says.** The issuer's block, signed with the database's
  root key, names who holds it, a key prefix, the columns, the operations,
  a purpose, an expiry, and the one branch its writes go to. That branch
  is never `main`: such a token is not issued, and every token also
  refuses writes to `main`. So an agent's writes always land on a branch,
  and the agent cannot choose which. Reads are not tied to the branch.
- **Who holds a token is read from the issuer's block only**, so a holder
  cannot rename themselves or move their branch by appending facts. The
  name is what the database records as the author of their commits.
- **A person makes the root key and issues tokens** with the command-line
  tool (`umbrachyx keygen`, `umbrachyx token issue`). The private key goes
  to a file only its owner can read and is never printed; a key file is
  never overwritten, since every token issued under it would stop
  working. Tokens travel as base64 text on standard input and output, not
  on the command line, where other users of the machine could see them.
- **Narrowing, not widening.** A holder appends a block of checks: a
  shorter key prefix, one column, read only, an earlier expiry. The
  database's allow policy reads only the facts of the issuer's block and the
  request, so facts a holder appends cannot satisfy it; a test appends the
  issuer's own fact names with wider values and is still refused. Asking to
  narrow to something wider adds a check that the original scope still
  fails.
- **Refusals carry one of four reasons:** invalid (not signed by this
  root, or damaged), expired, a write to `main`, or out of scope. Expiry is
  reported before scope, since renewing is the first thing to do. The caller
  is not told which fact failed, so a refusal does not reveal what the token
  would have allowed.
- **Evaluation has explicit limits.** Biscuit's defaults allow 1 ms of
  evaluation, which ordinary tokens exceeded on a loaded machine: in a test
  run with the CPU saturated, 6 of 200 runs refused a valid request. The
  time limit is now 50 ms, only to stop tokens built to be expensive; the
  limits on facts and iterations, which do not depend on the machine, stay
  at biscuit's defaults. A request that reaches a limit is refused.
- **Keys are strings for now.** Biscuit can test a string for a prefix but
  not a byte string; the database's keys are bytes, so they will need an
  encoding, such as hex, that keeps prefixes.

### What it costs

On the same MacBook Air, checking one request takes 58 µs at p50 and 107 µs
at p99 for an issued token of 432 bytes, and 85 µs and 103 µs once narrowed
(560 bytes). Most of it is verifying the signatures and evaluating the
Datalog. That is far more than a read from memory, so a token will be
verified once per session and the decision cached per scope, not repeated
for every key. This is a problem to solve, not a reason to leave biscuit:
macaroons, the other well-known scheme that narrows offline, need every
checker to hold the shared secret that mints tokens.

## The store

- **The log is the truth and the tree is rebuilt from it.** A commit is
  applied to the tree only after its record is synced, and opening the
  store replays the log into a new tree. Nothing the tree holds can be
  missing from the log.
- **A failed commit leaves the tree unchanged**, and the store, like the
  log, refuses further commits until it is reopened.
- **Group commit runs on one thread that owns the store.** Writers on other
  threads send it commits and wait for their answer; it writes every commit
  that arrived during the previous sync as one group, with one sync, and
  answers each writer once the group is durable. A commit to an unknown
  branch is refused alone, and a commit too large for one record is
  written alone, so neither fails the writers grouped with it. When the
  previous group had several writers, it waits up to 50 µs for each further
  commit, 0.5 ms in all: without that, the writers it just answered miss
  the next sync and the writers split into two groups taking turns, which
  doubled every wait with 8 writers. A single writer never waits.
- **Readers on other threads read a view the committer publishes.** A view
  is each branch's name and the root of its tree; trees never change once
  built, so taking one copies a pointer per branch. After writing a group,
  and before answering its writers, the committer swaps the shared view
  for a new one. A reader holds the lock only to copy that pointer, never
  while the committer syncs, and a writer that has been answered reads its
  own write back.
- **Every commit keeps the tree it produced**, so any past state can be
  read. Versions share the nodes they have in common, so keeping all of
  them costs only the paths each commit rewrote.
- **A commit is named by a BLAKE3 hash** of its parent, branch, provenance,
  time and edits. The hash of the tree is left out, because each cell in
  the tree names the commit that wrote it.
- **A branch is a name for a commit.** Making one writes a record and copies
  nothing. Commits to a branch are not seen on the branch it came from.
- **Each value is a cell**: the value, or a mark that it was removed, and
  the commit that last changed it. A removal stays in the tree as a mark,
  so "when, and in which commit, did this disappear?" is answered from the
  current state. Removing a key that is not there records nothing.
- **The history of a key follows those marks back.** The cell names the
  commit that last changed it; reading the key in that commit's parent
  gives the change before, and so on. The cost is the number of times the
  key changed, not the number of commits.
- **Diff, merge and undo work on values.** A cell rewritten with the value
  it already had is not a change. Each reads the tree diff, so its cost
  follows the number of changed keys.
- **A merge is three-way.** Changes to different keys combine, the same
  change on both sides is taken once, and different changes to one key are
  conflicts. Conflicts are reported with the base, ours and theirs values,
  and nothing is written; resolving them is left to the caller. A merge
  commit records the head it merged as a second parent.
- **The base of a merge is a shared commit that is not an ancestor of
  another shared commit.** Walking back from one side and taking the first
  shared commit found is wrong once that side has merged the other in: an
  older shared commit can be as near as the right one, and a test caught
  exactly that. With criss-cross merges several can qualify, and the latest
  is taken. Finding it costs the size of the shared history, which is not
  bounded yet.
- **Undoing a commit writes its opposite** to a branch whose history holds
  it: each key it changed goes back to the value before it. If any of
  those keys has changed since, nothing is written and each such key is
  reported as a conflict. This is how one wrongly granted reward is taken
  back without touching anything else.

## The server

- **It speaks the Redis protocol (RESP2) for `GET`, `SET` and `PING`.**
  Existing clients work with it, and `valkey-benchmark` can measure it
  and Valkey with the same tool over the same loopback socket, which is
  what makes read latencies comparable.
- **It serves one branch, never main.** Writes from a client are not
  trusted, so they land where a person can review the diff before merging
  it. Each `SET` is one commit, acknowledged once durable, with the
  client's address as its author.
- **It listens only on 127.0.0.1**, because nothing authenticates a client
  yet. Capability tokens are meant to guard it before it listens anywhere
  else.
- **One thread per connection, at most 1,024 connections, no async
  runtime.** It is the simplest design that serves the benchmarks, and it
  adds no dependency. A connection past the limit is told so and closed.
  A failure to accept a connection or to start its thread costs that
  connection only. Idle connections are not timed out yet. The limit can
  be raised with `serve --max-connections`. Measured against Valkey on
  Linux, this design served 10,000 connections with slowest reads at about
  half of Valkey's, so it stays for now. On macOS it holds up to about
  1,000 connections; at 5,000 its slowest reads are five times Valkey's,
  and past the thread limit (8,192 a process) it cannot serve at all.
  Serving that many there, or far more anywhere, would need connections
  that do not each hold a thread.
- **Every length a client sends is checked before anything is allocated
  for it**: at most 1,024 arguments, and at most 1 MiB for all the
  arguments of one command together. That bounds what a connection
  buffers, about 1 GiB over 1,024 connections, and keeps every `SET` well
  inside one log record. Nothing of a command is copied until all of it
  has arrived, so a command sent in many pieces costs its length once.
  Replies are sent whenever 1 MiB of them is waiting. A malformed request
  is answered with an error and the connection is closed, because where
  the next request starts is no longer known.
- **Commands sent together are answered together, in order.** The `SET`s
  among them are sent to the committer without waiting, so they share
  syncs the way writes from different connections do. Any other command
  first waits until the `SET`s before it are durable, so a `GET` reads what
  the same client set just before it, and replies stay in order.
- **History is read over the same server, with commands named `UMBRA.`**
  (`BRANCHES`, `CELLS`, `HISTORY`, `LOG` and `DIFF`), each answered with
  one JSON string written as the MCP tools write it. A person's tool, such
  as a database viewer, needs the history while the server runs, and the
  server's process is the only one that may open the log: a second process
  opening it would be a second writer, and opening a log repairs a torn
  end, which is a write. The commands read any branch, main included,
  since a branch is reviewed against main. They write nothing. Like `GET`,
  they are guarded only by listening on this machine, so anyone who can
  reach the port also sees who changed what and why; values of erased
  subjects stay unreadable here as everywhere.
- **The store answers them on the committer's thread, between groups of
  commits.** A view holds each branch's tree but not the commits, and
  copying every commit into every view would cost each write group the
  whole history. A question is sent down the same channel as commits, so
  it sees every commit sent before it, and waits behind their sync. That
  is fine for a person reading, and why `GET` still reads from a view.
- **Cells, changes and commits come a page at a time, 1,000 at most.**
  Commits wait while a question is answered, so no question may walk a
  whole branch or a whole history. A page of cells names the key to go on
  from, and the next page starts with one walk down the tree to it. A key's
  history and a branch's log give their newest page and say whether older
  entries were left out.

## The MCP session

- **One agent, one token, over standard input and output.** The agent's
  host starts `umbrachyx mcp --public-key K --token-file F` and exchanges
  JSON-RPC messages with it, one per line. The token is read once, at the
  start; a token not signed by the database's root key does not start a
  session. Nothing listens on the network.
- **Who the agent is and where it writes come from the token**, never from
  its requests: the token's holder is the author of every commit, and its
  branch is the only one written to. The agent is not asked for a
  purpose; the token's purpose is the one checked.
- **Every tool call is checked against the token for every key**, before
  anything is read or written. A check costs about 60 µs, which is small
  next to an agent's own round trip, so decisions are not cached yet.
- **The column is the last part of a key**, as in `player/42/gems`, since
  tokens name columns and the store holds keys.
- **Refusals are tool results, not protocol errors**, so the agent reads
  the reason: `refused:` followed by one of the four reasons a token gives.
  A diff leaves out the keys the token cannot read instead of refusing,
  so an agent sees its own part of a wider change; a token that allows
  nothing at all, such as an expired one, is refused.
- **Reads go to the agent's own branch once it exists**, otherwise to
  `main`, unless the call names another branch.
- **One tool writes: `commit`.** It takes the values to set, the keys to
  remove and a reason, and no branch: the commit goes to the token's
  branch, made from `main` the first time, with the token's holder as
  author. An argument it does not know, such as a branch, is refused
  rather than ignored. Every key is checked before anything is written,
  so a commit with one key outside the token writes nothing, not even the
  branch. A reason is required, since it is the history's answer to
  "why". Merging into `main` is not a tool: a person reviews the branch
  and merges it with the command-line tool, naming who approved it.
- **A message is at most 1 MiB**, as a command to the Redis server is. A
  longer line is answered with an error and skipped without being held
  whole, and the session goes on.
- **What a hijacked agent cannot do is tested and published** in
  [`agents.md`](agents.md), with the test for each attack.
- **Erased values are marked, and erasing is not a tool.** `get`,
  `history` and `diff` return an erased value as `null` with
  `"erased": true`. Destroying a subject's key is a person's decision,
  made with the command-line tool. The Redis server answers an erased
  value as nil, since its protocol has nothing else to say.
- **The session opens the log itself**, and the log takes one writer at a
  time, so one agent works on a database file at a time for now. Serving
  several agents at once would go through the committer, as the Redis
  server does.

## Erasing a subject

Values are encrypted under a key per subject, so that destroying the key
erases the subject while the history keeps its shape (see
[`concept.md`](concept.md)).

- **Values are sealed where they enter the store and opened where they
  leave it.** The log, the tree, history, diffs and merges hold only
  sealed values; a merge or an undo copies them without opening them.
  The command-line tool, the Redis server and the MCP session all open
  the store with its key file, `<log>.keys`. Keys stay in the clear,
  since they are how history and tokens find data; values do not.
- **Sealing is deterministic** (AES-SIV): one value under one key always
  seals to the same bytes, so equal values stay equal and the tree,
  diffs, three-way merges and undo work as before. What this gives away
  is whether a key's value is the same as at some other point, which the
  history's shape already shows. The key's bytes are sealed in as
  associated data, so a sealed value cannot be moved under another key.
- **A sealed value names its key by fingerprint**, so opening the store
  checks that every key the log needs is in the key file before anything
  is written, and refuses the log otherwise. A log written without
  encryption is refused too: a store holds sealed values only.

- **A subject is the first two parts of a key**: `player/42/gems` belongs
  to `player/42`. A key with one part, such as `config`, names no subject
  and shares one key for the whole database, so it cannot be erased on its
  own. Giving each such key a key of its own would cost a sync of the key
  file for every new key.
- **Opening every value costs reads about a fifth of their throughput**
  with 50 clients on the laptop, and a durable write about a tenth
  ([`bench/results/2026-11-05-sealed`](../bench/results/2026-11-05-sealed/)).
  Each read sets up AES-SIV for its subject's key, and `aes-siv` expands
  the counter-mode key inside every operation, so keeping the cipher
  between reads would save only part of it. Whether to accept the cost,
  cache opened values, or seal only the columns that need erasing is open.
- **The first value of a new subject costs a sync of the key file**, on
  top of the log's, made on the committer's thread. Writes that create
  many subjects at once pay it once each; batching them is left until it
  shows in a measurement.
- **The keys live in their own file next to the log** (`<log>.keys`),
  because the log is append-only and keeps every byte. The key file is the
  one place where something is destroyed: erasing writes it again without
  the subject and renames it over the old file, so a crash leaves the old
  file or the new one, never a mix.
- **A new key is synced before anything is encrypted under it**, so a crash
  never leaves ciphertext whose key was lost.
- **Opening the key file only reads it.** A torn entry at the end, left by
  a crash while a key was appended, is cut when the next key is written,
  after the store has checked that every key its log uses is present. A
  damaged entry with whole entries after it is refused as damage rather
  than read as a torn end: otherwise one flipped bit in a length would
  make every key after it look torn and be cut, which would erase those
  subjects.
- **A key file write that fails leaves the file to be replaced whole.**
  A failed append may have left part of its entry; appending the next key
  after it would bury the torn bytes where they read as damage, and the
  store would not open. So the next write replaces the file with the keys
  in memory instead, as it does after a torn end found when opening.
- **Bytes that do not make a whole entry are a torn end if no whole entry
  follows them**, whatever they hold: a torn append can come out as
  garbage, its length included. If a whole entry, with a length within
  the limit and a matching checksum, follows them, they are damage, and
  the file is refused, since the keys after them may be in use. Damage in
  the last entry therefore reads as a torn end; if the log uses that key,
  the store refuses to open for the missing key before the file is cut.
- **Each key has a fingerprint**, eight bytes of a BLAKE3 key derivation
  from it, so ciphertext can name the key it needs without revealing it,
  and a value whose key was erased can be told apart from a damaged one.
- **Erasing logs first, then destroys the key.** The log records the
  subject, which key, who erased it, why and who approved it, with no
  value; then the key file is rewritten without the key. If the rewrite
  does not happen, the next open finishes it, after the log has checked
  out against the key file. The key is taken out of memory before the
  file is rewritten, so nothing opens under it from that moment, on any
  thread. It stays out if the rewrite fails: a value written to the
  subject afterwards gets a new key, rather than one under the old key
  that finishing the erasure would destroy with the rest.
- **An erased value reads as erased, not as removed.** A cell, a history
  entry and a diff say so, and keep their commit, time, author and
  reason. Writes to the subject afterwards get a new key; the old values
  stay unreadable. A merge conflict on an erased value shows it as
  absent, without saying it was erased.
- **What erasing does not reach**: copies kept below the file system, such
  as blocks a solid-state disk has not reused yet, and backups of the key
  file. Keeping the key file out of backups, or encrypting it under a key
  held elsewhere, is left to the operator for now.

## Retention per column (not built yet)

The concept keeps every change of some columns, compacts others to one
entry per interval, and keeps only the current value of others. None of
that is built, for one reason: the log is the truth and is append-only,
so a value written to it stays in it. Keeping less history than the log
holds means writing a new log without the dropped changes and switching
to it (compaction), which touches the one part everything else rests on.

- **Until then, a column that must not keep its history can be erased.**
  A subject's values become unreadable without rewriting the log, which
  covers "this must be gone", though not "this takes too much space".
- **Compaction would come with its own crash tests**, like the log's: a
  kill at every step of writing the new log and switching to it must
  leave either the old log or the new one, and every acknowledged write
  that the policy keeps.
- **What a policy means across branches is open**: whether compacting a
  column on `main` also compacts it on a branch that has not merged yet.
