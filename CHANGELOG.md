# Changelog

## Unreleased

- **Backup and restore.** `umbrachyx backup <directory>` copies the log
  and the keys as they are at one moment, and adds only what was written
  since when run again; `umbrachyx restore <directory>` writes a new
  store from it and checks every record on the way. A backup stopped
  part way stays what it was before. An erasure reaches a backup with
  the next one begun after it. `umbrachyx serve --backup-to <directory>
  [--backup-every <seconds>]` keeps such a backup up to date while the
  server runs: a backup holds the moment it began, the log is copied a
  part at a time between writes, and no client can ask for one or name
  the place.
- **A server that follows another.** `umbrachyx serve <branch> --follow
  <host:port>` serves a restored backup of another server as its
  replica: it asks that server, on the port it was given with
  `--feed-replicas-on <port>`, for the
  records after the ones it holds, answers reads with the same commits
  under the same ids, and refuses writes with `READONLY`.
  `UMBRA.REPLICATION` says how far it is, and why it stopped if it did;
  `umbrachyx replication [--host <address>] [--port <port>]` asks a
  running server the same from the command line, opens no log, and
  exits with 1 if a replica stopped or cannot reach its primary, or a
  primary refuses writes because no replica keeps up.
  A replica holding another log, more records than its primary, or
  records its primary does not hold is sent nothing. For trials only:
  nothing is measured on two machines yet.
- **TLS between a server and its replicas, and keys that travel on
  it.** `--replication-cert`, `--replication-key` and `--replication-ca`
  name three PEM files: the server's certificate, its private key, and
  the certificate of the authority that signed the other servers'. Given
  to both, the servers speak TLS 1.3 on the replicas' port, each shows
  its certificate, and neither answers one whose certificate that
  authority did not sign; a replica also requires its primary's to name
  the host in `--follow`. On such a connection a replica is sent the
  keys of subjects first written after its backup, and reads them.
  Without the options nothing between the servers is encrypted, no key
  is sent, and `serve` says so when it starts. `UMBRA.REPLICATION` has
  `encrypted`. A replica sent values whose key was destroyed before it had
  them asks for everything through the erasure, up to 16 MiB at once.
  Not yet handled: more than that between a value and its erasure
  (`TOOFAR`), and an erased subject written again in the same answer as
  one erased for good; either stops the replica, which then has to be
  made again from a new backup.
- **A replica that keeps current values only.** `umbrachyx serve
  <branch> --follow <host:port> --current-values-only` holds in memory
  every value as it is now and nothing of the past, so what it holds
  does not grow with every write: 40 bytes for a SET to a key it
  already has, against 676 for a replica that keeps the history (one
  laptop). It answers `GET` and `SCAN` as before and every question
  about the past with `NOHISTORY`. Its log on disk is whole: started
  without the option it answers those too.
- **Opening a store takes less memory.** The log is replayed a record
  at a time instead of decoded whole first: a store opened from a 107 MB
  log held 1,313 MB right after opening and now holds 946 MB, in the
  same time (one laptop). A log that is refused is left as it was
  found, a torn tail included.
- **Answering a write only once a replica holds it.** `umbrachyx serve
  --feed-replicas-on <port> --wait-for-replica <seconds>` holds each
  answer until a replica has said the write is on its disk. A write no
  replica held in time is answered `NOTREPLICATED` (it is in this
  server's log); the writes after it are answered `NOREPLICA` and not
  written, until a replica holds everything. Puts to `plain-async`
  tables do not wait.

## 0.2.0

The first tagged version: a single node, to try the design on. From this
version on, a later version opens the log this one wrote; the layout of
the log may still change. The key file kept beside the log and the
protocol extensions (`UMBRA.*`) are not stable yet. Measured for release
on 2026-11-17 on AWS and Google Cloud at commit `20b1673`
(`bench/results/2026-11-17-aws-m6id.xlarge/` and
`bench/results/2026-11-17-gcp-n2-standard-4/`); the commits after it
change documents and the version number only.

- **A durable log is the truth.** A write is acknowledged only once it is
  synced to an append-only log (except to a table declared `plain-async`,
  below); the tree and the history are rebuilt from
  it on open. Group commit puts many writers into one sync.
- **History for every value.** Commits record who, why and who approved;
  branches, diffs, three-way merges (conflicts are detected, not resolved)
  and reverts work on a Merkle prolly tree.
- **Engines per table.** A table can be declared to keep only its latest
  values, held in a HAMT, for about a fifth of the memory per write.
  Tables without a declaration keep their full history.
- **Tables that keep no history.** A table declared `plain` holds the
  current value of each key and nothing else: no commits, no branches, no
  review, one value read on every branch, changed in place. Declared
  `plain-async`, a `SET` is answered before it is on disk and written
  within `--sync-every` milliseconds (a second by default); a power cut
  loses the last writes answered and nothing else. An agent's token still
  decides which keys it may write there, through the MCP `put` tool or
  the server's `SET`.
- **Erasure.** Values are encrypted per subject (AES-SIV); destroying a
  subject's key makes its values unreadable while the shape of the history
  stays.
- **A server for one branch** that speaks enough of the Redis protocol
  (RESP2: `GET`, `SET`, `PING`, `SCAN`, `TYPE`, `TTL`) for existing
  clients, plus `UMBRA.LOG`, `UMBRA.HISTORY`, `UMBRA.CELLS`, `UMBRA.DIFF`,
  `UMBRA.BRANCHES` and `UMBRA.TABLES` for the history. It listens on
  127.0.0.1 only.
- **An MCP session for agents**, authorized by biscuit capability tokens:
  an agent reads only what its token allows and writes only to its own
  branch, never to `main`. [`docs/agents.md`](docs/agents.md) lists what a
  hijacked agent was tested not to be able to do.
- **A command-line tool**, `umbrachyx`, for the log, keys, tokens, tables
  and the server. [`docs/demo.md`](docs/demo.md) walks through it.
- **Robustness tests**: the server killed mid-write 100 times, the log
  killed 1,000 times, and 10,000 deterministic simulation seeds, on Linux
  and Windows in CI.
- **Numbers**, with the slower rows included, are in
  [`docs/comparison.md`](docs/comparison.md) and
  [`bench/results/`](bench/results/).
