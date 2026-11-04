# Umbrachyx

**An in-memory database that keeps every change as history.**

- **Reads never wait for writes.** While 50 clients write durably, a `GET`
  answers in 15 µs: about 650 times sooner than Valkey with every write
  synced to disk.
- **Nothing acknowledged is lost.** Killed mid-write 1,100 times with
  334,631 writes acknowledged, and put through more than 280,000 simulated restarts
  after power cuts and disk faults: not one acknowledged write went
  missing.
- **Every change is kept**, with who made it and why, to branch, diff,
  merge and undo.

Measured on one laptop, against Valkey and Dolt, with the raw output
published. [Where it is slower](#numbers-so-far) is in the same table.

Umbrachyx is meant to read and write at memory speed. Behind every value sits
its full history: when it changed, from what, to what, who changed it, why,
and who approved the change. That history is kept per record and per cell,
and you can branch it, diff it and merge it, the way git handles files.

The name comes from *umbra* (shadow) and *tachyon* (the hypothetical particle
faster than light). The current value is the light. Its history is the shadow
that is already there before the light arrives.

> **Status: prototype.** A first single-node core exists so the design can be
> tried: a durable log, a tree with branches, diffs and merges, a
> command-line tool, and a server for one branch that speaks enough of the
> Redis protocol for existing clients. [`docs/demo.md`](docs/demo.md) walks through the first
> four questions below with it. A first comparison with Valkey and Dolt,
> and what it shows still needs work, is in
> [`docs/comparison.md`](docs/comparison.md). No format is stable. Do not
> build on it. The design starts at [`docs/concept.md`](docs/concept.md).

## Numbers so far

Apple M4 MacBook Air (32 GB, internal SSD), one machine, 2026-11-03 and
2026-11-04. Every write on both sides is on disk before it is acknowledged
(Valkey with `appendfsync always`). Synthetic data, 32-byte values,
1,000,000 keys present. Latencies are medians unless marked p99.

| | Umbrachyx | Compared with | Roughly |
|---|---|---|---|
| `GET` while 50 clients write | 0.015 ms | Valkey 9.86 ms | **~650× faster** |
| `GET`, one client | 0.015 ms | Valkey 0.055 ms | **~3.5× faster** |
| Durable writes, 256 writers at once | 30,773 /s | Valkey 24,301 /s | **~1.3× more** |
| Diff of 10 changes in 10,000,000 records | 9 µs | Dolt 96 µs | **~10× faster** |
| Loading 10,000,000 records | 7.1 s | Dolt 36 s | **~5× faster** |
| `GET` p99, 1,000 connections | 6.1 ms | Valkey 9.2 ms | ~1.5× faster |
| One durable write | 4.0 ms | Valkey 4.0 ms | the same: the disk's flush |
| `GET` p99, 5,000 connections | 170 ms | Valkey 36 ms | **~5× slower** |
| 10,000 connections | fails | Valkey 139,782 `GET`/s | **not served yet** |

Read the first two rows with care. Over a socket, a `GET` costs what a
`PING` costs for both servers, so they compare how each server handles a
connection, not how fast it finds a value. The first row is a real
difference in design: Valkey answers on one thread, after the sync of the
writes around it, while Umbrachyx reads a published snapshot that a write
replaces by swapping one pointer. The last two rows are the cost of one thread per connection,
which Umbrachyx will have to give up to serve many clients. The 256-writer
row measures Umbrachyx in process and Valkey over loopback; the diff row
includes Dolt's SQL layer.

How each number was taken, and what it does not show:
[`docs/comparison.md`](docs/comparison.md). The raw output, machine and
versions: [`bench/results/`](bench/results/). The scripts are in
[`bench/`](bench/), and CI runs them against Valkey on every change.

## Robustness

| Test | Runs | Result |
|---|---|---|
| Kill the server with SIGKILL while 16 clients send `SET`s (half pipelined) | 100 | 330,912 acknowledged, none lost |
| Kill a process appending to the log | 1,000 | 3,719 acknowledged, none lost |
| Simulated disk: power cuts tearing writes at any sector, failed syncs, bit flips | 10,000 seeds | 130,677 restarts; every acknowledged record back, 6,867 corruptions refused |
| Simulated store: commits, branches, merges and reverts through crashes and a clock that goes back | 10,000 seeds | 635,173 commits, 152,927 restarts; every branch as acknowledged |

CI runs all four on Linux and Windows for every change. The server test
and the simulations were also run against deliberately broken versions
(acknowledging before the write is durable, skipping a sync, misreading
damage) and caught each of them. Output:
[`bench/results/2026-11-04/robustness.txt`](bench/results/2026-11-04/robustness.txt).

Not tested yet: power cuts on real hardware (only simulated), runs longer
than minutes, and more than one node. Umbrachyx has no replication yet.
A restart replays the whole log, about 15 µs per commit.

## What it is for

The first target is **game operations**: items, in-game currency and player
state. These need memory-speed access, and they also raise questions a plain
cache cannot answer:

- "My item disappeared." *When, and through which change?*
- "This account gained currency it should not have." *By what path?*
- "We granted the wrong reward to everyone." *Undo exactly that grant, and
  nothing else.*
- "Try the new event against real data." *On a branch, without touching
  production.*
- "Delete my account." *Make that player's data unreadable for good, while the
  rest of the history stays intact.*

## Principles

1. **Do not trust the caller.** AI agents are fast, numerous and fallible, and
   they can be hijacked. Every caller holds a scoped capability (which rows,
   which columns, until when, for what purpose), and least privilege is built
   into the structure rather than left to configuration.
2. **Agents write to branches.** A person reviews the diff and merges it. Any
   point in time can be restored.
3. **Every value knows where it came from:** who wrote it, from which session
   or instruction, and who approved it.
4. **Deleting means destroying the key.** Data is encrypted per subject. To
   erase a subject, its key is destroyed: the shape of the history stays, and
   the content can never be read again.
5. **Trust comes from evidence, not age.** Formal models, deterministic
   simulation testing and published test results replace "it has run for
   years".

## Relationship to dbboard

[dbboard](https://github.com/meta-taro/dbboard) is a desktop SQL editor and
viewer from the same author. The two are separate products in separate
repositories. dbboard will reach Umbrachyx through an adapter, and its history
views (a row's history, a cell-level diff, who last changed a row) are being
designed so that they work against Umbrachyx as well.

## License

[MIT](LICENSE)
