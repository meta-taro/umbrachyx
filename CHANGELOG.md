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
  `UMBRA.REPLICATION` says how far it is, and why it stopped if it did.
  A replica holding another log, more records than its primary, or
  records its primary does not hold is sent nothing. For trials only:
  nothing on the way is encrypted, keys are not sent, a write is
  acknowledged before any replica has it, and nothing is measured yet.

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
