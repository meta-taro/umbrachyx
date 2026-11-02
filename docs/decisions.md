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
- **A record is one commit**: the branch, the keys it sets or removes, who
  made it, why, who approved it and when. It is one frame, so a commit that
  touches many keys is in the log whole or not at all. The payload starts
  with a format version; a frame whose checksum holds but which does not
  decode, whatever its version, stops the log from opening rather than
  being cut as a torn write, because a torn write cannot pass the checksum.
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
  restart the tree must hold exactly what the acknowledged commits wrote, an
  interrupted commit must be there whole or not at all, and recorded times
  must not go back. CI runs 10,000 seeds of each, in release mode; the
  results are the same as in debug mode.

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
  root key, names a key prefix, the columns, the operations, a purpose and
  an expiry. Every token also refuses writes to `main`, so an agent's
  writes always land on a branch.
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
