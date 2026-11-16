# Changelog

## 0.1.0

The first tagged version: a single-node prototype to try the design on.
No format is stable yet, neither the log, the key file nor the protocol
extensions; a later version may not open what this one wrote.

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
