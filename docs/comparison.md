# Comparison with Valkey and Dolt

A first measurement, taken early so that the design can be reviewed while
it is still cheap to change. It is not a claim that Umbrachyx is fast:
where it is not, this page says so, and where the numbers cannot be
compared, it says that too.

## Setup

- Apple M4 MacBook Air (fanless, 32 GB), macOS 26.6.2, internal SSD with
  APFS, on AC power, 2026-11-03. One machine; nothing else heavy running.
- Umbrachyx at the commit that adds this page, built with `--release`.
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
| Umbrachyx, commit of one key (in process) | 4.02 | 5.89 |
| Valkey, `SET` with `appendfsync always` (loopback) | 4.01 | 4.91 |

Both cost what the disk's flush costs, about 4 ms on this machine, so a
single writer gets about 250 durable writes a second from either. Umbrachyx
is slower at p99. Valkey's own default (`appendfsync everysec`) and its
setting without persistence acknowledge before the data is on disk; they
are faster and can lose acknowledged writes, which Umbrachyx is designed
not to do.

## Reads

| | p50 (µs) | p99 (µs) |
|---|---|---|
| Umbrachyx `get`, 100,000 records (in process) | 0.42 | 0.92 |
| Umbrachyx `get`, 1,000,000 records (in process) | 0.88 | 1.88 |
| Umbrachyx `get`, 10,000,000 records (in process) | 1.46 | 4.42 |
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
| 10,000,000 | 10 | 96 |

Umbrachyx calls `Store::diff` in process; the median of 21 runs. Dolt runs
`SELECT COUNT(*) FROM dolt_diff('main', 'test', 'inventory')` 200 times in
one `dolt sql` process, minus the same number of `SELECT 1`, divided by
200; the median of five such runs. Dolt's figure includes parsing and
planning a SQL query, which Umbrachyx does not have. Dolt also stores its
data in a prolly tree, and both grow only slightly with 100 times more
records.

## Loading

| Records | Umbrachyx (s) | Dolt `table import` (s) |
|---|---|---|
| 1,000,000 | 8.6 | 3 |
| 10,000,000 | 103 | 36 |

Umbrachyx loads in commits of 50,000 records and applies each record to
the tree one at a time, rehashing the leaf it lands in every time. That is
about 10 µs per record at 10,000,000 records, nearly three times Dolt.

## What this says about the design

1. **One durable write costs one flush, about 4 ms here.** That is the
   same for Valkey and bounds any database that acknowledges only durable
   writes to a few hundred writes a second per writer. The way past it is
   group commit: writes that arrive together share one flush. This is the
   first thing to build before any write throughput is claimed.
2. **Applying a write to the tree costs about 10 µs at 10,000,000
   records.** It is not on the path that waits for the disk, but it bounds
   throughput once group commit takes the flush off each write, and it is
   far from memory speed. Applying a commit's keys as one sorted batch,
   and hashing each changed node once per commit instead of once per key,
   are the next things to try.
3. **Reads cannot be compared until Umbrachyx serves them over a socket.**
   Speaking enough of the Redis protocol for `valkey-benchmark` to drive
   it would make the comparison exact.
4. **Diffs already cost what the change costs**, 10 µs for ten changes in
   10,000,000 records, which is the property the tree was chosen for.

Memory use has not been compared yet.
