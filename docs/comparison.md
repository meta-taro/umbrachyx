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
  cargo run --release -p umbrachyx --example concurrent_writes
  bench/dolt.sh /tmp/dolt-input
  bench/valkey.sh
  bench/umbrachyx.sh
  bench/connections.sh
  bench/memory.sh
  cargo run --release -p umbrachyx --example old_reads
  ```

- The raw output of the latest runs, with the machine and versions, is in
  [`bench/results/`](../bench/results/).

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

### Many writers at once

Each writer sends one write, waits until it is durable, then sends the
next. Umbrachyx writers are threads sending to its committer, which writes
every commit that arrived during the previous sync as one group with one
sync (2,048 commits per run, to an empty store). Valkey's clients are
`valkey-benchmark -c N` over loopback (20,000 `SET`s per run); with
`appendfsync always` it also syncs once for all the writes it read in one
pass of its event loop.

| Writers | Umbrachyx (writes/s) | p50 / p99 (ms) | Valkey (writes/s) | p50 / p99 (ms) |
|---|---|---|---|---|
| 1 | 248 | 4.01 / 4.62 | 248 | 4.01 / 4.91 |
| 8 | 1,924 | 4.05 / 5.14 | 984 | 7.99 / 10.0 |
| 64 | 10,691 | 6.00 / 6.72 | 7,313 | 8.59 / 14.3 |
| 256 | 30,773 | 7.94 / 8.28 | 24,301 | 10.3 / 13.5 |

Over three runs, Umbrachyx's rate moved by up to 16% at 64 and 256
writers; Valkey's was measured once, and its one-writer row is the run
in the table above. Umbrachyx's numbers have no network
in them and Valkey's do, but the loopback round trip is about 0.06 ms
against waits of 4 ms and more.

With 8 writers both first took two syncs per write: right after a sync,
the next one started before the writers just answered had sent again, so
the writers split into two groups taking turns. Valkey shows that pattern
above. Umbrachyx now waits up to 50 µs for one more commit, at most 0.5 ms
in all, when the previous group had more than one writer; a single writer
never waits. That brought 8 writers from about 1,070 writes a second and
8 ms to the numbers in the table.

## Reads

### Over the Redis protocol

Both servers driven by the same `valkey-benchmark`, over loopback, with
1,000,000 keys present and 32-byte values. Umbrachyx runs `umbrachyx serve`
on a branch; its keys were written there with `SET`, each a durable commit.
One client, one request at a time, 100,000 requests per row.

| | Umbrachyx p50 / p99 (ms) | Valkey p50 / p99 (ms) |
|---|---|---|
| `PING`, the round trip alone | 0.015 / 0.023 | 0.047 / 0.063 |
| `GET` | 0.015 / 0.023 | 0.055 / 0.063 |
| `SET`, durable when acknowledged | 4.00 / 5.00 | 4.01 / 4.87 |

With 50 clients sending `GET`s at once, Umbrachyx answered 233,100 a
second (p50 0.119 ms, p99 0.151 ms) and Valkey 217,391 (0.183 ms,
0.439 ms).

**This still does not compare reading.** For both, `GET` costs what `PING`
costs: reading the value is lost in the round trip. What differs is the
round trip itself. Umbrachyx gives each connection its own thread, blocked
in a read until a request arrives; Valkey serves every connection from one
event loop. That is a difference in how the servers handle connections,
measured at one and at 50 of them, not one in how fast data is found; a
server with a thread per connection is expected to fare worse with
thousands of connections, which has not been measured. With 50 clients
the two rates are close, and `valkey-benchmark` itself runs on one thread,
so that row may show the limit of the client rather than of either server.
`valkey-benchmark` reports in coarse buckets, so small values are rounded.
Valkey's numbers are the run without persistence for `PING` and `GET` and
the `appendfsync always` run for `SET`; its `GET` with `appendfsync always`
was the same, 0.055 / 0.071 ms.

### While others write

The same `GET`s from one client, while 50 other clients keep sending
`SET`s, each durable before it is acknowledged (Valkey with `appendfsync
always`). The writers start a second before the readers and are stopped
after them. Both rows of a column come from one run of its script, so
`GET` alone differs slightly from the table above.

| | Umbrachyx p50 / p99 (ms) | Valkey p50 / p99 (ms) |
|---|---|---|
| `GET`, alone | 0.015 / 0.031 | 0.055 / 0.071 |
| `GET`, while 50 clients `SET` | 0.015 / 0.039 | 9.86 / 11.12 |

Valkey runs every command on one thread, and with `appendfsync always` it
syncs the writes it has read before it replies to anything read with
them, so a `GET` waits for the sync of the `SET`s around it. Umbrachyx
reads from the view the committer last published, on the connection's own
thread, so a `GET` does not wait for a sync. Its `GET` is still current
for the client that sends it, because the client's own `SET`s are answered
only once they are in that view. In a separate check of the same run,
Umbrachyx's writers made about 9,500 durable `SET`s a second while the
`GET`s ran, and were still writing when they ended. Umbrachyx's `GET`
rate rose while others wrote (74,738 a second, against 58,106 alone); why
was not measured.

### Many connections

`GET`s from 1,000, 5,000 and 10,000 clients at once, each waiting for its
reply before the next request, 500,000 requests per row
(`bench/connections.sh`). The client runs on four threads. Each row waits
40 s first, so the ports of the previous row's connections are free again.

| Clients | Umbrachyx (GET/s) | p50 / p99 (ms) | Valkey (GET/s) | p50 / p99 (ms) |
|---|---|---|---|---|
| 1,000 | 180,571 | 2.65 / 6.07 | 165,837 | 3.51 / 9.19 |
| 5,000 | 152,765 | 22.1 / 170 | 179,211 | 21.2 / 35.6 |
| 10,000 | failed | | 139,782 | 41.5 / 472 |

This is where a thread per connection shows. At 5,000 clients Umbrachyx's
median matches Valkey's but its p99 is about five times Valkey's: 5,000
threads take turns on ten cores. At 10,000 it could not serve at all.
macOS allows a process 8,192 threads, so 1,810 connections got no thread
and were closed, and the benchmark stopped. On macOS, 1,000 or so is what
Umbrachyx serves well. Linux does not have that limit; see below.

### On a Linux CI runner

The same scripts run in CI on a GitHub-hosted Linux runner (Ubuntu 24.04,
four virtual CPUs) for every change. That machine is shared with other
work, so its numbers compare runs of the job rather than serve as the
tables above; two runs of 2026-11-04 are kept in
[`bench/results/2026-11-04-linux-ci/`](../bench/results/2026-11-04-linux-ci/).
Ranges below span both runs; the connection rows are from the second.

| | Umbrachyx | Valkey |
|---|---|---|
| `GET`, one client, p50 / p99 (ms) | 0.031 / 0.055–0.063 | 0.031 / 0.055 |
| `GET` while 50 clients `SET`, p50 / p99 (ms) | 0.031–0.039 / 0.087–0.095 | 0.78–0.86 / 1.53–6.39 |
| `GET`s a second, 50 clients | 66,138–73,314 | 92,937–127,389 |
| Durable `SET`s a second, one client | 2,228–2,912 | 3,139–4,351 |
| `GET`, 1,000 connections: a second, p99 (ms) | 94,020, 10.5 | 99,721, 18.4 |
| `GET`, 5,000 connections: a second, p99 (ms) | 75,199, 62.2 | 62,112, 147 |
| `GET`, 10,000 connections: a second, p99 (ms) | 75,838, 133 | 68,157, 279 |

A sync costs about 0.3 ms here against 4 ms on the laptop, so a Valkey
`GET` that waits for the writes around it waits less, and Umbrachyx's lead
there is 20 to 28 times rather than 650. Linux let Umbrachyx start a
thread for each of 10,000 connections, and its slowest reads stayed at
about half of Valkey's at every size: the limit on macOS was the
platform's thread cap, not one thread per connection as such. With one
durable writer and with 50 clients reading, Valkey is ahead on this
machine; why was not investigated yet.

### In process

| | p50 (µs) | p99 (µs) |
|---|---|---|
| Umbrachyx `get`, 100,000 records | 0.42 | 0.83 |
| Umbrachyx `get`, 1,000,000 records | 0.88 | 1.54 |
| Umbrachyx `get`, 10,000,000 records | 1.50 | 3.92 |

Reading a value in the tree costs about 1 µs, under a tenth of the
loopback round trip above.

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
10,000,000 records. (Nodes have since stopped being hashed on writes and
are hashed when a diff first needs them; the load was not measured again.)

The first measurement applied each record on its own and rehashed the leaf
it landed in every time: 8.6 s for 1,000,000 records and 103 s for
10,000,000, about 10 µs per record.

## What this says about the design

1. **One durable write costs one flush, about 4 ms here.** That is the
   same for Valkey and bounds a single writer to about 250 durable writes
   a second. Group commit lets writers that arrive together share one
   flush: 256 writers reach about 30,000 durable writes a second. Past
   that, applying each commit to the tree on the committer's one thread
   is the next limit to measure.
2. **Applying a write to the tree cost about 10 µs at 10,000,000
   records**, which would have bounded throughput once group commit takes
   the flush off each write. Applying each commit as one sorted batch,
   with each changed node hashed once, brought a large commit down to
   about 0.7 µs per record. A commit of a single key still rebuilds and
   hashes one node per level.
3. **Over a socket, a read costs the round trip.** Behind the same
   protocol and the same client, `GET` costs what `PING` costs for both
   servers, so the tree is not what limits a read. How connections are
   served is. While others write durably, a read in Umbrachyx still costs
   the round trip, because it reads a published view instead of waiting
   for the committer; in Valkey it waits for a sync, about 10 ms on the
   laptop and under 1 ms on the Linux runner. A thread per connection
   holds up on Linux, where 10,000 connections had slowest reads at about
   half of Valkey's; on macOS, at 5,000 the slowest reads are five times
   Valkey's, and the thread cap rules out 10,000.
4. **Diffs already cost what the change costs**, 9 µs for ten changes in
   10,000,000 records, which is the property the tree was chosen for.

## Memory

Umbrachyx loses here, by far. The same SETs of 32-byte values over a
space of 200,000 keys, twice over, as `bench/memory.sh` sends them; the
resident size of each server process, the median of three runs in a Linux
virtual machine on the laptop. The first row is the store as it was until
it stopped keeping a whole tree for every commit
([`2026-11-09-linux-vm/`](../bench/results/2026-11-09-linux-vm/)), the
second as it is now
([`2026-11-09-linux-vm-sparse-trees/`](../bench/results/2026-11-09-linux-vm-sparse-trees/)):

| | After 200,000 SETs | After 400,000 SETs | Added per SET in the second pass |
|---|---|---|---|
| Umbrachyx, a tree per commit | 1.4 GiB | 2.9 GiB | about 8 KB (7.7 to 8.2 KB) |
| Umbrachyx, a tree per 1,024 commits | 484 MiB | 1.1 GiB | about 3.2 KB (3.1 to 3.2 KB) |
| Valkey 9.1.3 | 17 MiB | 20 MiB | 17 bytes |

The second pass writes keys that are already there. Valkey replaces the
value in place, so it barely grows. Umbrachyx keeps every SET as a commit
with the cell it wrote and the one it replaced. A whole tree per commit
cost more than that: each copied the path from the root to the changed
key, about four nodes of a few dozen entries each. Now one commit in 1,024
keeps its tree and past trees are rebuilt from it, so reading the past
costs more: a diff between two old commits that are not parent and child
took about 7 ms on average and 16 ms at worst in the same machine, where
it took under 0.1 ms (`cargo run --release -p umbrachyx --example
old_reads`, output in the same folder). A commit against its parent is
read from its own cells and stays under 0.1 ms.

At this rate a million SETs, as `bench/umbrachyx.sh` writes to fill its
key space, need about 3 GB. Much of what is left is the history itself,
each commit and its cells, which keeping fewer trees does not remove.
Only a retention policy that drops or folds old changes would cut that
(see "Retention per column" in [`decisions.md`](decisions.md)); it is
designed but not built.
