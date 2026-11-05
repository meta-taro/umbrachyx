# Umbrachyx

**An in-memory database that keeps every change as history.**

- **Reads never wait for writes.** While 50 clients write durably, a `GET`
  answers in tens of microseconds: about 650 times sooner than Valkey with
  every write synced to disk on a laptop, and 20 times sooner on a Linux CI
  runner, where a sync costs less.
- **Nothing acknowledged is lost.** Killed mid-write 1,100 times with
  334,631 writes acknowledged, and put through more than 280,000 simulated restarts
  after power cuts and disk faults: not one acknowledged write went
  missing.
- **Every change is kept**, with who made it and why, to branch, diff,
  merge and undo.

Measured on a laptop and on a Linux CI runner, against Valkey and Dolt,
with the raw output published. [Where it is slower](#numbers-so-far) is in the same table.

Umbrachyx is meant to read and write at memory speed. Behind every value sits
its full history: when it changed, from what, to what, who changed it, why,
and who approved the change. That history is kept per record and per cell,
and you can branch it, diff it and merge it, the way git handles files.

The name comes from *umbra* (shadow) and *tachyon* (the hypothetical particle
faster than light). The current value is the light. Its history is the shadow
that is already there before the light arrives.

> **Status: prototype.** A first single-node core exists so the design can be
> tried: a durable log, a tree with branches, diffs and merges, a
> command-line tool, a server for one branch that speaks enough of the
> Redis protocol for existing clients, and an MCP session through which an
> agent reads only what its capability token allows. [`docs/demo.md`](docs/demo.md) walks through the first
> four questions below with it. A first comparison with Valkey and Dolt,
> and what it shows still needs work, is in
> [`docs/comparison.md`](docs/comparison.md). No format is stable. Do not
> build on it. The design starts at [`docs/concept.md`](docs/concept.md).

## Numbers so far

Two machines, 2026-11-03 and 2026-11-04: an Apple M4 MacBook Air (32 GB,
internal SSD, macOS), and a GitHub-hosted Linux runner (Ubuntu 24.04, four
virtual CPUs) shared with other work, over two runs. Every write on both
sides is on disk before it is acknowledged (Valkey with `appendfsync
always`). Synthetic data, 32-byte values, 1,000,000 keys present.
Latencies are medians unless marked p99. Umbrachyx comes first in each
pair; an empty cell was not measured on that machine.

| | Laptop (macOS) | Linux CI runner |
|---|---|---|
| `GET` while 50 clients write | **~650× faster** (0.015 vs 9.86 ms) | **~20–28× faster** (0.031–0.039 vs 0.78–0.86 ms) |
| `GET` p99, 10,000 connections | **not served** (macOS thread limit) | **~2× faster** (133 vs 279 ms) |
| `GET` p99, 5,000 connections | **~5× slower** (170 vs 36 ms) | **~2.4× faster** (62 vs 147 ms) |
| `GET` p99, 1,000 connections | ~1.5× faster (6.1 vs 9.2 ms) | ~1.7× faster (10.5 vs 18.4 ms) |
| `GET`, one client | ~3.5× faster (0.015 vs 0.055 ms) | the same (0.031 vs 0.031 ms) |
| `GET`s a second, 50 clients | about the same (233,100 vs 218,818) | **~1.4–1.7× fewer** (66,138–73,314 vs 92,937–127,389) |
| One durable write | the same (4.0 vs 4.0 ms): the disk's flush | **~1.4–1.5× fewer a second** (2,228–2,912 vs 3,139–4,351) |
| Durable writes, 256 writers at once | ~1.3× more (30,773 vs 24,301 a second) | |
| Diff of 10 changes in 10,000,000 records, vs Dolt | **~10× faster** (9 vs 96 µs) | |
| Loading 10,000,000 records, vs Dolt | **~5× faster** (7.1 vs 36 s) | |

The rows compare against Valkey unless they say Dolt. How much faster
depends on the machine. The first row is a real difference in design:
Valkey answers on one thread, after the sync of the writes around it,
while Umbrachyx reads a published snapshot that a write replaces by
swapping one pointer. So the gap is as wide as a sync is slow: about 4 ms
on the laptop, about 0.3 ms on the runner. Over a socket, a `GET` alone
costs what a `PING` costs for both servers, so the one-client and
50-client rows compare how each server handles a connection, not how fast
it finds a value. Umbrachyx serves each connection on its own thread:
macOS caps a process at 8,192 threads, so 10,000 connections fail there,
while Linux serves them. Where Valkey is ahead on the runner (one writer,
50 readers) was not investigated yet. The runner is noisy: its two runs
differ by up to a third. The 256-writer row measures Umbrachyx in process
and Valkey over loopback; the diff row includes Dolt's SQL layer.

How each number was taken, and what it does not show:
[`docs/comparison.md`](docs/comparison.md). The raw output, machine and
versions: [`bench/results/`](bench/results/) (the laptop in `2026-11-04/`,
the runner in `2026-11-04-linux-ci/`). The scripts are in
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
