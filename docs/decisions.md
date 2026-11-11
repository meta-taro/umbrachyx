# Decisions

Technical choices that shape the code, and why they were made. A technology
without an entry here is not adopted.

## Dependencies

| Crate | Used for | Why this one |
|---|---|---|
| `crc32fast` | Checksum on every log record, so a torn or damaged record is detected on recovery | Widely used and maintained, MIT OR Apache-2.0, no dependencies beyond `cfg-if`. Hardware-accelerated where available, so the check costs little on the write path. A checksum is too small a thing to justify writing and verifying our own |
| `rustix` (dev, Unix only) | A plain `fsync` in the sync latency example, to compare it with `F_FULLFSYNC` | std has no way to call a plain `fsync` on macOS. Maintained by the Bytecode Alliance, Apache-2.0 / MIT, and safe to call, where `libc` would need `unsafe`. Not linked into the library |
| `blake3` | The hash that names every node of the tree, so equal hashes mean equal content and a diff can skip shared subtrees | Collision resistance is required: a diff or a sync between replicas trusts that equal hashes mean equal data. CC0 / Apache-2.0, maintained by its authors. |
| `biscuit-auth` | Capability tokens: what a caller may read or write, carried by the caller | Tokens can be narrowed offline by whoever holds them and never widened, and are verified with a public key, so nothing secret sits on the side that checks them. Writing an authorization scheme ourselves would need its own security review. Apache-2.0, maintained under the Eclipse Foundation. Default features are off; `datalog-macro` stays on because 6.0.0 does not build without it. Through `p256` it pulls in `crypto-bigint` 0.5.5, which has a timing advisory (RUSTSEC-2026-0387); Umbrachyx signs only with Ed25519 and P-256 is only ever verified, on public data, so the advisory is ignored in `.cargo/audit.toml` until `biscuit-auth` moves to a fixed version |
| `serde_json` | Reading and writing the JSON-RPC messages of the MCP session | The de facto JSON crate for Rust, MIT OR Apache-2.0, maintained by the serde authors. It was already built as a dependency of `biscuit-auth`, so it adds no crate. Used through `Value` only, without derive macros. The MCP protocol itself is written here rather than taken from the official Rust SDK, which runs on an async runtime the server does not use; the part served is four methods |
| `getrandom` | The random bytes of each subject's encryption key | The operating system's random source, nothing else: the smallest way to get key material. MIT OR Apache-2.0, maintained by the rust-random project, and already built as a dependency of `biscuit-auth` at the same version, so it adds no crate |
| `aes-siv` | Encrypting each value under its subject's key, so that destroying the key erases the subject | Deterministic authenticated encryption (RFC 5297) from RustCrypto, Apache-2.0 OR MIT, maintained with the rest of its AEADs. Deterministic is what the store needs: equal values must stay equal once encrypted for the tree, diffs and merges to work unchanged, and AES-SIV is built to be used that way rather than with a nonce we would have to invent. Default features are off; the random source is `getrandom`, already a dependency. It brings its RustCrypto building blocks (`aes`, `cmac`, `ctr` and their traits) |
| `mio` | Asking the operating system which of many sockets are ready, so a few threads serve every connection | epoll, kqueue and Windows' completion ports behind one small interface, and nothing else: no runtime, no executor. Written against each by hand it would be three implementations to keep correct. MIT, maintained by the Tokio project and the layer Tokio itself stands on; features `os-poll` and `net` only |
| `tikv-jemallocator` (binary only, not on Windows) | The `umbrachyx` binary's memory allocator | Every write copies and hashes tree nodes, each hash into a fresh buffer. With glibc's malloc, the C library took 12% of the server's time under SETs; with jemalloc, 6%, and pipelined SETs ran 10 to 18% faster in a Linux virtual machine ([`bench/results/2026-11-09-linux-vm-node-hash/`](../bench/results/2026-11-09-linux-vm-node-hash/)). jemalloc is the allocator Valkey and Redis build with, maintained by the TiKV project, MIT / Apache-2.0 for the Rust part and BSD-2-Clause for jemalloc. It is C built from source, so building needs a C compiler and `make`. mimalloc was measured too and was no faster without the change to hashing; it was not tried with it. Memory per write did not change with either. The library and its tests keep the system allocator; it does not build with the MSVC toolchain, so Windows keeps it too |

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
- **A server's log is zeroed ahead of its records, 16 MiB at a time.** A
  sync of a file that grows waits mostly for the file system, which has to
  record the new length and the new blocks; the record's own bytes are the
  small part. A record written over zeros that are already on the disk
  changes neither, so the sync has only those bytes to make durable. In a
  Linux virtual machine an append and its sync took 152 µs on a growing
  file and 46 to 55 µs on a zeroed one, and over the server a durable `SET`
  from one client went from about 5,000 a second to about 10,300, where
  Valkey with `appendfsync always`, whose file grows, did about 5,150
  (the release machine has not measured this yet). The promise is the one
  above: nothing is acknowledged before its sync returns. The zeros and
  the file's new length are synced when each 16 MiB is reserved, before
  any record goes into it.
  - **The end of the records is found as a torn end always was.** No
    record has a length of zero, so the zeros check out as no record, and
    no whole record starts among them. A write a power cut tore now sits
    in front of zeros instead of at the end of the file; it is cut the same
    way, by zeroing it in place, and the reserve stays.
  - **A record that ends in zero bytes and lost only those reads as
    written**, because over zeros it is the same bytes. It was never
    acknowledged, and an unacknowledged record may survive a crash, as
    before.
  - **Half the seeds of the log's simulation run on such a file**, through
    the same power cuts, torn sectors and bit flips; reading an all-zero
    end as damage fails the second seed.
  - **Only `serve` reserves.** Other commands write once and leave; they
    open the log as it is and cut the zeros a server left, and the next
    server reserves again. Reserving costs one write of 16 MiB and a sync,
    a pause of some milliseconds every few hundred thousand writes, which
    will show in the slowest writes; reserving ahead on another thread
    would remove it and is not built.
  - **What it does not change:** on a file system that can show stale
    blocks after a crash, a file that grew could already end in bytes of
    an older file. Zeroing and syncing before use narrows that to a crash
    during the reserving itself.
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

- **The log stays binary; `umbrachyx export` writes it out as JSON Lines.**
  One record a line, in log order, so `jq` and `grep` can search the whole
  history: commits with their changes, branches, tables, rejections and
  erasures. A log of JSON itself was not taken: values are any bytes and
  are encrypted, so they would be hex or base64 either way, each write
  would pay for the text, and a line cut short by a crash still needs a
  length and a checksum to tell it from damage. The export opens values
  with the key file; a value whose subject was erased is `null` with
  `"erased": true`. It opens the log like any other command, so it is
  refused while a server holds the log.

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
- **Hashing nodes is most of a write's work**, about 42% of the server's
  time under SETs in a Linux virtual machine. A node's fields are gathered
  into one buffer and hashed with one call, which gives the same hash as
  feeding them one by one: BLAKE3 hashes whole 1 KiB chunks in parallel
  with SIMD only when given them together. That took pipelined SETs from
  about 50,500 to 61,500 a second there
  ([`bench/results/2026-11-09-linux-vm-node-hash/`](../bench/results/2026-11-09-linux-vm-node-hash/)).
  Nodes under 1 KiB still hash one block at a time.
- **A node is hashed only when a diff first needs its hash**, and the hash
  is kept. Most nodes a write makes are replaced by a later write before
  anything compares them, so they are never hashed. In the same virtual
  machine, pipelined SETs went from about 68,000 to 168,000 a second and
  SETs one at a time from about 33,600 to 49,900; memory did not change.
  The cost moves to diffs: the first diff that reaches a tree hashes what
  nobody has hashed yet, and a diff between old commits took 5.4 ms on
  average instead of 4.3 ms on a laptop
  ([`bench/results/2026-11-09-linux-vm-lazy-hash/`](../bench/results/2026-11-09-linux-vm-lazy-hash/)).
  No node is hashed more often than before. If a workload that diffs a lot
  finds the first diff too slow, the hashing can be moved to a background
  thread or back onto writes.
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
- **A token may approve, or write, never both.** `approve` lets its
  holder merge a change into `main`, checked for every key the change
  touches against the token's key prefix and purpose. A token is not
  issued with both `write` and `approve`, so nobody passes their own
  change, and an agent's token, which writes, cannot approve at all. A
  token that does not write names no branch.
- **A token's signatures are checked once, when a connection or a session
  is given it; each request is still decided by biscuit.** Checking the
  signatures was most of the 60 to 85 µs a decision took from a token's
  bytes, and a connection decides one for every write: pipelined `SET`s
  from an authenticated connection ran at about 78,000 a second where an
  unauthenticated one ran at 145,000. The verified token is kept and each
  request evaluated against all its blocks, as before, at about 130,000 a
  second. Reading the token's bounds out once and comparing keys ourselves
  would be faster still, and would be an authorization scheme of our own;
  a holder can append any check to a token, and only biscuit reads them
  all.
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
- **Group commit has no thread of its own: whoever waits for a commit
  writes it, with every commit queued by then.** Commits are queued in the
  order sent. A writer that waits and finds nobody writing takes the queue
  and writes it with one sync, its own commit and everyone else's; the
  others sleep until theirs is answered, and what arrives during that sync
  is the next writer's group. A commit to an unknown branch is refused
  alone, and a commit too large for one record is written alone, so
  neither fails the writers grouped with it.
  - **It replaced a thread that owned the store.** Handing a commit to
    that thread and being woken with the answer cost a lone writer two
    wake-ups, 20 to 40 µs, which was little beside a sync on a growing
    file and a third of everything once the log was reserved. In a Linux
    virtual machine a durable `SET` from one client went from about 10,300
    a second to about 12,100, and from eight clients to about 44,000,
    where Valkey with `appendfsync always` did about 5,500 and 19,000.
  - **The thread also waited up to 50 µs for further commits** when a
    group had several writers, so they would not split into two groups
    taking turns. That wait is gone: with a sync this short it cost more
    than it gathered.
  - **A commit sent and never waited for** is written by the next writer,
    the next question about the history, or the stop.
  - **Where it loses:** with 256 clients the server did about 80,000
    durable `SET`s a second and Valkey about 128,000. Each connection has
    a thread, and 256 of them on two processors spend their time being
    scheduled; that is the server's design, not the commit's.
- **Readers on other threads read a view the committer publishes.** A view
  is each branch's name and the root of its tree; trees never change once
  built, so taking one copies a pointer per branch. After writing a group,
  and before answering its writers, the committer swaps the shared view
  for a new one. A reader holds the lock only to copy that pointer, never
  while the committer syncs, and a writer that has been answered reads its
  own write back.
- **Every past state can be read, and no commit keeps a whole tree.**
  Only the head of each branch has one. Every commit keeps the cells it
  wrote and the cells those replaced, which is enough to go either way
  along a line of history:
  - **A past tree** is made when asked for, from the head of its branch by
    putting back what was written since, or from nothing by replaying
    what was written before, whichever is nearer.
  - **A diff between two commits on one line** is read from the cells
    written between them, with no tree. It costs the commits in between.
  - **A key's value at an old commit** is found along the key's own
    chain: each cell names the commit that wrote it, and that commit holds
    the cell it replaced. It costs the times the key was written since.
  - **A merge** reads what the incoming branch changed and looks each key
    up at the head it merges into, so it does not cost how far that
    branch has moved.

  Earlier forms kept trees. A tree per commit copied the path from the
  root to every key it changed, about 8 KB per one-key commit. One tree
  per 1,024 commits took about 3.2 KB a commit, and most of that was
  still the trees: counted by the allocator over 200,000 one-key commits
  to 1,000,000 keys (`cargo run --release --example memory_cost`), a
  commit left 3,405 bytes held, of which about 635 were the commit and
  its cells. A kept tree holds its own copy of every leaf changed since
  the one before it, and a leaf is 32 entries copied whole for one
  changed key, so spacing alone gave back little: 4,145 bytes a commit
  at every 256, 2,282 at every 8,192. With no kept trees a commit leaves
  619 bytes, and over the Redis protocol a SET takes 620 bytes of
  resident memory where it took about 3,200. A key written only once
  costs 242 bytes.

  Reading the past got faster where a viewer reads it and slower in one
  place, measured on a laptop over 200,000 commits
  ([`bench/results/2026-11-12-laptop-no-kept-trees/`](../bench/results/2026-11-12-laptop-no-kept-trees/)):
  a diff against the fifth commit before went from 5.1 ms to 0.007 ms,
  against the thousandth from 4.9 ms to 0.44 ms, one key's value at an
  old commit from 0.048 ms to 0.002 ms. **The whole state of an old
  commit went from 3.7 ms to 43.5 ms**, and it grows with the history,
  where it used to be bounded by 1,024 commits. The same holds for a
  diff between commits on different branches when neither is a head.
  **Look again if** a command people use reads whole old states often:
  a few trees remembered after they are made would bound it.
- **No hash index of current values beside the tree** (measured, not
  taken). A table that keeps its history answers a `GET` by walking the
  prolly tree; an index of current values would answer it with one
  lookup, as a table declared `latest` already does. The latest table is
  therefore the most such an index could give, and it gives little. In
  the process, with no network and 1,000,000 keys, a read costs about
  560 ns from the tree and 410 ns from the hash table
  (`cargo run --release --example read_cost`, on a laptop); the tree's
  figure was 880 ns before nodes kept their keys in one buffer. Over the
  Redis protocol, in a Linux virtual machine with 500,000 keys and three
  runs each, `GET`s per second from a history table and from a latest
  table were 33,400 to 33,900 against 31,300 to 33,700 from one client,
  272,000 to 316,000 against 244,000 to 305,000 from 50 clients, and
  1.33 million against 1.36 million from 50 clients sending 16 at a
  time: no difference but about 2% in the last. A request costs far more
  in the round trip than in the lookup. The index would also add memory
  per key, which is where the store is furthest behind. **Look again
  if** a profile of `GET` shows the tree walk above a tenth of the
  server's time.
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
- **`SCAN`, `TYPE` and `TTL` are answered too, so a viewer can list keys.**
  Database viewers that speak Redis find keys with `SCAN` and open each
  after asking its type and time to live. Every value is a string and
  none expires, so `TYPE` and `TTL` answer as Redis does for such a key.
  They read the latest view, as `GET` does. A `SCAN` cursor counts the
  entries of the branch's tree already walked, because Redis clients
  expect a number; finding where it points reads the entries before it,
  and a key added or removed before it during a walk can make the walk
  return a key twice or miss one, which Redis allows for keys changed
  during a walk.
- **It serves one branch, never main.** Writes from a client are not
  trusted, so they land where a person can review the diff before merging
  it. Each `SET` is one commit, acknowledged once durable.
- **`AUTH <token>` (or `AUTH <user> <token>`, as Redis takes a password)
  names who writes.** Started with the database's public key, the server
  checks a capability token as the MCP session does: its holder becomes
  the author of the connection's writes, and every `SET` is checked
  against the token's branch, keys, columns, purpose and expiry before it
  is sent (`NOPERM` otherwise). A bad token is `WRONGPASS` and leaves the
  connection as it was. A connection without `AUTH` writes as
  `unauthenticated`; `serve --require-auth` refuses its writes
  (`NOAUTH`), and reads stay open either way.
- **It listens only on 127.0.0.1**, because reads need no token and writes
  need one only with `--require-auth`.
- **A few threads serve all the connections, each waiting on many sockets
  at once; no async runtime.** A thread asks the operating system which of
  its sockets have something to read, reads each and carries out the
  commands, queueing the `SET`s, and sends the replies that are ready.
  There are as many threads as
  processors, up to four (`Server::threads`), and a new connection goes to
  the next in turn. At most 1,024 connections unless `serve
  --max-connections` says otherwise; one past the limit is told so and
  closed. Idle connections are not timed out yet.
  - **It replaced a thread per connection**, which was the simplest
    design and added no dependency, and which was woken for every request
    and put back to sleep after it. In a two-processor Linux virtual
    machine, `GET`s from 50 and from 256 clients went from about 220,000
    and 200,000 a second to about 320,000 and 300,000, where Valkey did
    326,000 and 305,000; durable `SET`s from 256 clients from about 78,000
    to about 120,000, where Valkey did 128,000. It also ends the thread
    limit that kept macOS from serving 10,000 connections.
  - **A serving thread never waits for a sync; one more thread does.** It
    writes the `SET`s every serving thread queued, as one group with one
    sync, and wakes them to send those replies. The first version had each
    serving thread wait for its own `SET`s, which saved handing them over
    and made every other connection on that thread wait for the sync too:
    a `GET` while 50 clients wrote, the row this server is built for, went
    from a slowest percent of 0.04 ms to 19 ms on the laptop. A test now
    holds a sync for 400 ms on a server with one serving thread and expects
    another connection's `GET` answered long before. With the writing
    thread, in the virtual machine: that `GET` at a median of 0.006 ms and
    a slowest percent of 0.047 ms, where Valkey's are 0.5 ms and 4.2 ms.
    The hand-over costs a lone writer about 20 µs: a durable `SET` from one
    client runs at about 9,800 a second over the server, not the 12,100 of
    a writer that writes for itself, and Valkey's at about 5,200.
  - **A connection's own commands stay in order.** One that is not a `SET`
    is not carried out until the `SET`s before it are answered, so it reads
    what they wrote; the `SET`s after a `SET` are queued without waiting.
  - **What it cost:** a `GET` from one client went from about 32,000 a
    second to about 28,600, the same as Valkey's 28,700. A thread that
    blocks on its one socket is woken straight into the read; a thread
    waiting on many asks which first. Keeping a thread spinning for the
    next request would win that back and would burn a processor on a
    quiet server, so it is not done.
  - **Reads are answered on every serving thread at once**, from the view;
    only writes take turns. That is where more processors than these
    measurements had should show.
  - **A connection whose client does not take its replies is not read
    from until it does**, so what waits for it stays under about 1 MiB,
    and a client that sends without pause gives the others their turn
    after each 1 MiB read.
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
  (`BRANCHES`, `TABLES`, `CELLS`, `HISTORY`, `LOG` and `DIFF`), each answered with
  one JSON string written as the MCP tools write it. A person's tool, such
  as a database viewer, needs the history while the server runs, and the
  server's process is the only one that may open the log: a second process
  opening it would be a second writer, and opening a log repairs a torn
  end, which is a write. The commands read any branch, main included,
  since a branch is reviewed against main, and `BRANCHES` names the branch
  the server writes to. They write nothing. Like `GET`,
  they are guarded only by listening on this machine, so anyone who can
  reach the port also sees who changed what and why; values of erased
  subjects stay unreadable here as everywhere.
- **The store answers them in a writer's turn, between groups of
  commits.** A view holds each branch's tree but not the commits, and
  copying every commit into every view would cost each write group the
  whole history. A question takes the turn to write and writes the queue
  first, so it sees every commit sent before it, and waits behind a sync. That
  is fine for a person reading, and why `GET` still reads from a view.
- **Cells, changes and commits come a page at a time, 1,000 at most.**
  Commits wait while a question is answered, so no question may walk a
  whole branch or a whole history. A page of cells names the key to go on
  from, and the next page starts with one walk down the tree to it. A key's
  history and a branch's log give their newest page and say whether older
  entries were left out. A diff gives its first page of keys and says
  whether more differ; it is still worked out whole, which costs the
  number of changes, not the size of the branches.

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
- **Every read takes one lock around the encryption keys, and stays
  so.** In process, with no network, it is what the latest table's
  quicker lookups wait on: on a 10-core laptop with 50 threads its 99th
  percentile read took 6.1–6.6 µs against the history's 3.3 µs
  (`examples/read_contention.rs`,
  [`bench/results/2026-11-10-laptop-read-order`](../bench/results/2026-11-10-laptop-read-order/)).
  Each thread keeping the last few keys it used, under a lock of its own
  that an erasure wipes, took that to about 1.8 µs. It was not kept:
  over the server a read takes about 110 µs and the 50-client rows did
  not move, and it would have put process-wide state on the path that
  makes an erasure hold for every reader at once. It is worth taking
  again if a workload shows readers waiting on that lock.
- **The first value of a new subject costs a sync of the key file**, on
  top of the log's, **and commits written together share it.** The keys
  of every new subject in a group are made first and made durable with
  one write, before any value is sealed: either all are in use afterwards
  or none, since a value sealed under a key that is not durable could
  never be opened. It showed in a measurement: pipelined `SET`s that each
  brought a new subject ran at 241 a second on the laptop, one sync of the
  key file each, and at about 70,000 with the keys made together. A group
  with a new subject still waits for two syncs, the key file's and then
  the log's.
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

## Approving changes

An agent's writes land on its branch, and a person decides what reaches
`main`. Today that person has to know branches, diffs and merges. A
viewer that tried it asked for less: a list of changes waiting, the diff,
and approve or reject. This is the shape agreed with that viewer before
building (`store/approvals.rs`, `server/review.rs`); the viewer's screen
for it is the gate for the first public release, with the speed work.

- **A change waiting is a branch with commits `main` does not have**,
  not approved or rejected yet. Every branch counts, not only agents', so
  a person's experiment on a branch shows up the same way.
- **`UMBRA.PENDING`** lists them: who wrote them and why, when the first
  and the last commit were made, the head commit's id, the base (the
  latest commit the branch shares with `main`, so that the diff from base
  to head shows what the branch changed and nothing `main` changed
  since), how many keys
  change in each table (the first part of the key), whether `main` has
  moved since the branch started, and whether merging would conflict.
  The branch's name is returned but a viewer need not show it. Each
  change is one element, a flat list of field names and values, so a
  viewer that draws Redis replies as tables needs no conversion.
- **`UMBRA.APPROVE <branch> <head> <reason>`** merges the change into
  `main` only if the branch's head is still `<head>`: a person approves the diff they
  saw, and anything an agent wrote after it is refused, not merged
  unseen. A conflict merges nothing and returns the conflicting keys.
- **`UMBRA.REJECT <branch> <head> <reason>`** closes the change without
  merging.
  The rejection is a record in the log with who and why, like an erasure,
  so the list and the history agree after a restart.
- **Who approves is the connection, not an argument.** A connection
  authenticates with a capability token, sent as Redis sends a password:
  `AUTH <token>` or `AUTH <user> <token>` (the user name is ignored), so
  a viewer that keeps a Redis password in its keychain can keep the token
  there unchanged; the token's holder is recorded
  as the one who approved or rejected. Approving needs an operation that
  is never granted together with a branch to write to, so an agent's
  token cannot approve its own change, or any other. A token that can
  approve cannot write; a person who also writes holds two tokens. The
  merge records its approver in `approved_by`. Replies give it as a list
  already, empty when nobody approved, so tools need not change when an
  approval takes two people; the log holds one name until then.
- **A connection that has not authenticated** reads as before and may
  `SET` on the served branch, recorded with `unauthenticated` as who
  rather than an empty name; it may not approve or reject.
  `serve --require-auth` refuses its writes as well; the default stays as
  it is for now.
- **This is the one way a server writes to `main`.** The server still
  serves one branch for `SET`; `APPROVE` writes a merge to `main` and
  nothing else, and only for a token that may approve.
- **The diff is `UMBRA.DIFF`.** Reading a large change a table and a page
  at a time is still to come.
- **`UMBRA.DECISIONS [count]`** lists the approvals and rejections,
  newest first, each one flat array like a change waiting: the decision,
  the branch, the head it was made on, who made it and why, when, and for
  an approval the merge it made. Whoever runs an agent wants to read why
  its change was turned down. An approval is found in the log as a merge
  into `main` that names who approved it; a merge nobody approved is not
  a decision.
- **Refusals say why in their first word**, as Redis errors do:
  `NOAUTH` without a token, `NOPERM` for a token that may not approve a
  key the change touches, `NOTWAITING` with a second word for why
  (`moved`: written to since; `decided`: approved or rejected already;
  `none`: never a change there), `CONFLICT` followed by the keys both
  sides changed. A viewer says something different for each without
  parsing the message.
- **`-` names the state before any commit** in `UMBRA.DIFF`, unless a
  branch is called so. A change whose branch shares no commit with `main`
  has no base; `UMBRA.DIFF - <head>` shows all it holds as added.
- **A change is named by its branch and head, not its head alone.** A
  branch made from another starts at the same head, and the store
  simulation found a rejection landing on the branch it came from when
  the head alone was given. A viewer passes the branch `UMBRA.PENDING`
  returned; a person need not see it.
- **Not in this version:** reopening a rejected change (an agent offers
  it again on a new branch) and approvals that need two people.

## Engines per table (being built)

Every value pays for the full history and the ordered tree today, and that
is where Umbrachyx loses to Valkey: about 3 KB kept per SET against
Valkey's 17 bytes, and a GET that walks the tree (315 ns) where a hash
table takes 31 ns. Some data needs neither: a player's position or a
session is read by key, and only its current value matters. So a table
can be declared to keep only its latest value, the way MySQL lets a table
choose its storage engine.

- **What differs is only how a table is held in memory. The log stays one
  log.** Every engine writes through it, and no write is acknowledged
  before it is synced, whatever the engine. One log also keeps a commit
  that touches tables of different engines atomic, and keeps the crash
  tests one set.
- **The log still holds everything**, whatever a table's engine drops
  from memory. Disk use and restart time do not change; reducing those is
  compaction (below), a separate step.
- **A table is declared before anything is written to it**, on any
  branch. Changing the engine of a table that has keys would mean
  rebuilding it from the log, which is not built, so it is refused; the
  store as written then stays equal to the store as the log replays.
- **A table is the first segment of a key**: `player/42/gems` is in the
  table `player`. The subject that erasing works on stays the first two
  segments.
- **Which engine a table uses is a record in the log**, so replaying the
  log gives the same tables every time. A table never declared keeps its
  history, as every table does today.
- **Two engines to begin with**:

  | | History (today's) | Latest |
  |---|---|---|
  | Held in memory | the prolly tree and the cells of every commit | a HAMT of current values |
  | GET | walks the tree | one hash lookup |
  | Range scan | in key order | reads every entry |
  | Past values and undo | all of them | none |
  | Branches | one pointer | one pointer (the HAMT is persistent too) |
  | Diff and merge | the tree's | between branch heads only; a three-way merge over the keys a branch wrote |

- **A latest table still branches.** An agent's writes always go to a
  branch, so an engine without branches could not take them. That is why
  it is a HAMT and not a plain hash table, which would be copied whole for
  every branch. Off `main`, a commit keeps the values it wrote and the ones
  they replaced, so a merge can tell a conflict: for each key that differs
  between the two branches now, the value where the branch first wrote it
  is compared with both sides. When one side is `main`, which keeps no
  such values, a differing key the branch did not write was changed on
  `main`. Those values are kept for good for now; dropping them once the
  branch is merged is not built.
- **On `main`, a latest table keeps no past.** A commit there keeps no
  cells for it, so a diff that reaches into the past of `main` cannot
  include it, and says which tables it left out. A diff between two
  branch heads compares the two HAMTs, and a commit off `main` against its
  parent uses the values it kept. Undo leaves latest tables as they are.
- **The HAMT carries no Merkle hashes.** A diff skips every subtree the
  two sides share by pointer, which is every untouched one when both come
  from the same line of history, as a branch and its base do; HAMTs built
  apart still diff correctly, reading more. Keys are placed by SipHash
  with a key drawn once per process, so keys chosen to collide cannot pile
  into one node; nothing leaves the process in that order, since every
  listing and diff is sorted by key. Proofs over a latest table would need
  the hashes back.
- **A table is declared from the command line**, `umbrachyx table pos
  latest --who W --why Y`, with who and why kept in the log like a commit.
  Not from the Redis server or MCP: what a table keeps is a decision for
  the people who run the store, and an agent's token reaches keys, not
  tables.
- **Tools can tell the tables apart.** `UMBRA.TABLES` answers
  `{"default":"history","tables":[{"name":"pos","engine":"latest"}]}`; a
  table not listed keeps its history. `UMBRA.HISTORY` on a key of a latest
  table answers `-NOHISTORY table 'pos' keeps only the latest value`: the
  first word of a Redis error names its kind, as `WRONGTYPE` does, so a
  tool can tell "no history kept" from a failure. `UMBRA.CELLS` leaves
  latest tables out and `UMBRA.DIFF` leaves them out where `main` kept no
  past; both name them in `omitted_tables`, an empty list when nothing was
  left out. The current values are still read with `SCAN` and `GET`.
  Reading a key of a latest table as an older commit left it is refused
  (`StoreError::NoPast`) rather than answered as empty: without its past,
  the table cannot tell a key that held nothing then from one it no
  longer remembers.
  `UMBRA.CELLS` names every latest table on every page, whatever the
  branch holds. The MCP `diff` tool names left-out tables too, but only
  those its token may read: for each table, it asks the token to read a
  key of it built from what the issuer granted (the key prefix and each
  column), so expiry and every narrowing count, and an expired token is
  refused. A token narrowed below its issuer's grant may not be told of a
  table it could read; it is never told of one it cannot.
- **Not built**: a way for others to plug in engines, and a log per
  engine.
- **No append-only engine for events, for now** (decided after the
  latest engine's numbers). Events are rows written once under new keys.
  A latest table already held those for about 780 bytes a SET where the
  history held about 2,400 when this was decided (since then the history
  stopped keeping trees and holds about as little on a branch; see "The
  store") (the first pass of `bench/memory.sh`, every
  key new), and an event that is never changed loses nothing by keeping
  only its latest value. What an append engine would add is reading rows
  in the order they were written, and nothing reads that way yet: the
  server's `SCAN` has no order, and there is no time-range command. It
  would also need its own answers for `UMBRA.HISTORY`, `UMBRA.CELLS` and
  `UMBRA.DIFF`, which tools would have to learn. **Build it when** a
  command or a user needs rows in written order, or when events in a
  latest table measure at more than half the memory per row of an append
  engine prototype.
- **Stop if** the latest engine does not at least halve the memory kept
  per SET in `bench/memory.sh`, or makes neither SET nor GET at least 10%
  faster; or if its branches fail the deterministic simulation or the
  hijacked-agent tests.
- **Measured on 2026-11-10** (`docs/comparison.md`, "Memory" and "A
  table that keeps only latest values"): about 630 bytes a SET against
  about 3,090 with history, a fifth. `GET` while 50 clients write was 11%
  faster, `GET` from one client 8%, `SET` the same, and `GET` from 50
  clients 4% slower with its slowest reads 3.6 times the history's. The
  simulation and the hijacked-agent tests pass. Memory and one `GET` row
  clear the bar, so the engine stays. The 50-client reads were explained
  the same day: they followed a long run of `SET`s, and run in the
  history's order they are 3% faster with about the same slowest reads
  ([`bench/results/2026-11-10-laptop-read-order`](../bench/results/2026-11-10-laptop-read-order/)).

## Retention per column (not built yet)

Engines per table (above) cover keeping only the current value. The rest
of this section is about keeping some history but less than all of it.

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
