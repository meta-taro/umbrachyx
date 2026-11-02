# Comparison with Valkey and Dolt

A first measurement, taken early so that the design can be reviewed while
it is still cheap to change. It is not a claim that Umbrachyx is fast:
where it is not, this page says so, and where the numbers cannot be
compared, it says that too.

## Setup

- Apple M4 MacBook Air (fanless, 32 GB), macOS 26.6.2, internal SSD with
  APFS, on AC power, 2026-11-03. One machine; nothing else heavy running.
- Umbrachyx at the commit that last changed this page, built with
  `--release`.
  Valkey 9.1.3 and Dolt 2.6.2 from Homebrew, default builds.
- The same synthetic data everywhere: a game inventory, keys like
  `player/00012345/slot/042` (24 bytes) and 32-byte values.
- How to rerun:

  ```sh
  cargo run --release -p umbrachyx --example compare -- 1000000 /tmp/dolt-input
  bench/dolt.sh /tmp/dolt-input
  bench/valkey.sh
  ```

## Writes that are durable when acknowledged

Every write is synced to disk before the caller is told it succeeded. For
Valkey this is `appendonly yes` with `appendfsync always`; Umbrachyx always
works this way. One client, one write at a time, 1,000,000 records present.

| | p50 (ms) | p99 (ms) |
|---|---|---|
| Umbrachyx, commit of one key (in process) | 4.00 | 4.72 |
| Valkey, `SET` with `appendfsync always` (loopback) | 4.01 | 4.91 |

Both cost what the disk's flush costs, about 4 ms on this machine, so a
single writer gets about 250 durable writes a second from either. Over 17
runs, Umbrachyx's p99 moved between 4.3 and 6.4 ms; Valkey's was measured
once. Valkey's own default (`appendfsync everysec`) and its
setting without persistence acknowledge before the data is on disk; they
are faster and can lose acknowledged writes, which Umbrachyx is designed
not to do.

## Reads

| | p50 (µs) | p99 (µs) |
|---|---|---|
| Umbrachyx `get`, 100,000 records (in process) | 0.42 | 0.83 |
| Umbrachyx `get`, 1,000,000 records (in process) | 0.88 | 1.54 |
| Umbrachyx `get`, 10,000,000 records (in process) | 1.50 | 3.92 |
| Valkey `GET`, 1,000,000 keys (loopback) | 55 | 63 |
| Valkey `PING`, the round trip alone (loopback) | 55 | 63 |

**These cannot be compared yet.** Umbrachyx has no server, so its numbers
contain no network round trip, and Valkey's are almost entirely the round
trip: `GET` costs the same as `PING`. A fair comparison needs Umbrachyx
behind a socket. `valkey-benchmark` reports in coarse buckets, so its
values are rounded.

## Diff of two branches that differ by ten records

Both build a branch, change ten records on it (five updated, five added),
and compare it with `main`. The ten changes are the same in both.

| Records | Umbrachyx (µs) | Dolt (µs) |
|---|---|---|
| 100,000 | 6 | 80 |
| 1,000,000 | 7 | 87 |
| 10,000,000 | 9 | 96 |

Umbrachyx calls `Store::diff` in process: 200 ms of untimed runs first, then
the median of 1,001. Without the warm-up the median moved between 8 and
46 µs from run to run, because the single commits measured just before each
wait on the disk and leave the core running slowly. Dolt runs
`SELECT COUNT(*) FROM dolt_diff('main', 'test', 'inventory')` 200 times in
one `dolt sql` process, minus the same number of `SELECT 1`, divided by
200; the median of five such runs. Dolt's figure includes parsing and
planning a SQL query, which Umbrachyx does not have. Dolt also stores its
data in a prolly tree, and both grow only slightly with 100 times more
records.

## Loading

| Records | Umbrachyx (s) | Dolt `table import` (s) |
|---|---|---|
| 1,000,000 | 0.7 | 3 |
| 10,000,000 | 7.1 | 36 |

Umbrachyx loads in commits of 50,000 records, each synced to disk. A commit
is applied to the tree as one sorted batch: every node above a changed key
is rebuilt and hashed once per commit. That is about 0.7 µs per record at
10,000,000 records.

The first measurement applied each record on its own and rehashed the leaf
it landed in every time: 8.6 s for 1,000,000 records and 103 s for
10,000,000, about 10 µs per record.

## What this says about the design

1. **One durable write costs one flush, about 4 ms here.** That is the
   same for Valkey and bounds any database that acknowledges only durable
   writes to a few hundred writes a second per writer. The way past it is
   group commit: writes that arrive together share one flush. This is the
   first thing to build before any write throughput is claimed.
2. **Applying a write to the tree cost about 10 µs at 10,000,000
   records**, which would have bounded throughput once group commit takes
   the flush off each write. Applying each commit as one sorted batch,
   with each changed node hashed once, brought a large commit down to
   about 0.7 µs per record. A commit of a single key still rebuilds and
   hashes one node per level.
3. **Reads cannot be compared until Umbrachyx serves them over a socket.**
   Speaking enough of the Redis protocol for `valkey-benchmark` to drive
   it would make the comparison exact.
4. **Diffs already cost what the change costs**, 9 µs for ten changes in
   10,000,000 records, which is the property the tree was chosen for.

Memory use has not been compared yet.
