# Umbrachyx

**An in-memory database that keeps every change as history.**

- **Every change is kept**, with who made it and why, to branch, diff,
  merge and undo: per key and per cell, the way git keeps files.
- **Agents write only to their own branch.** An agent's capability token
  names its branch; nothing it holds can write to `main`. A person reads
  the diff and merges it. [`docs/agents.md`](docs/agents.md) lists what a
  hijacked agent was tested not to be able to do.
- **Reads never wait for writes.** While 50 clients write durably, a `GET`
  answers in tens of microseconds, about 18 times sooner than Valkey with
  every write synced, on the machine releases are measured on.
- **Nothing acknowledged is lost.** Killed mid-write 1,100 times with
  843,005 writes acknowledged on the release machine, and put through
  more than 400,000 simulated restarts after power cuts and disk faults:
  not one acknowledged write went missing.

It is not a faster Valkey. For plain reads and writes Valkey is ahead
(about 1.3 times for one client, twice for 50), and keeping history costs
far more memory (about 3 KB a write against 16 bytes). Both are in
[the same table](#numbers-so-far) as the rows where Umbrachyx is ahead,
measured on AWS, a laptop and a Linux CI runner against Valkey and Dolt,
with the raw output published.

## Quick start

Five minutes, with the command-line tool: write, let an agent change a
branch, read the diff, and merge it as a person.

```console
$ cargo build --release && export PATH="$PWD/target/release:$PATH"
$ export UMBRACHYX_DB="$PWD/quick.log"
$ umbrachyx table pos latest --who you --why "positions need no past"
declared pos latest
$ umbrachyx commit main player/42/gems=100 pos/42/x=10 --who you --why "first data"
committed 186dd36a0364 to main
$ umbrachyx branch agent/support --from main
created agent/support from main
$ umbrachyx commit agent/support player/42/gems=150 --who agent:support --why "ticket 8812"
committed 63d11a6ea0f1 to agent/support
$ umbrachyx diff main agent/support
~ player/42/gems: 100 -> 150
$ umbrachyx merge main agent/support --who you --why "checked ticket 8812"
merged fae80a1d255d
$ umbrachyx history main player/42/gems
2026-11-11T07:43:46Z  fae80a1d255d  150  by you  "checked ticket 8812"
2026-11-11T07:43:45Z  186dd36a0364  100  by you  "first data"
$ umbrachyx history agent/support player/42/gems
2026-11-11T07:43:45Z  63d11a6ea0f1  150  by agent:support  "ticket 8812"
2026-11-11T07:43:45Z  186dd36a0364  100  by you  "first data"
```

- **Declare a table before its first write.** `pos` keeps only its latest
  values; once a table has keys, its engine can no longer be declared
  and `table` refuses. Undeclared tables keep their history.
- **On `main`, a merge is the change.** It is credited to whoever merged;
  the agent's own commit, with its reason, stays on its branch.
- Here the agent is played by hand with `--who`. A real agent gets a token
  for its branch and talks to `umbrachyx mcp`; [`docs/agents.md`](docs/agents.md)
  sets one up. To watch the same history in a viewer, `umbrachyx serve
  <branch>` speaks the Redis protocol with `UMBRA.*` commands for history.
- **No format is stable yet.** A later version may not open a log this
  one wrote; keep the commands that load your data, not the log.

Umbrachyx is meant to read and write at memory speed. Behind every value sits
its full history: when it changed, from what, to what, who changed it, why,
and who approved the change. That history is kept per record and per cell,
and you can branch it, diff it and merge it, the way git handles files.

The name comes from *umbra* (shadow) and *tachyon* (the hypothetical particle
faster than light). The current value is the light. Its history is the shadow
that is already there before the light arrives.

> **Status: prototype.** A first single-node core exists so the design can be
> tried: a durable log, a tree with branches, diffs and merges, values
> encrypted per subject so that a subject can be erased, a command-line
> tool, a server for one branch that speaks enough of the Redis protocol
> for existing clients and answers questions about the history, and an MCP
> session through which an agent reads only what its capability token
> allows and writes only to its own branch
> ([`docs/agents.md`](docs/agents.md) shows what a hijacked agent cannot
> do). [`docs/demo.md`](docs/demo.md) walks through all five questions
> below with it. A first comparison with Valkey and Dolt,
> and what it shows still needs work, is in
> [`docs/comparison.md`](docs/comparison.md). No format is stable. Do not
> build on it. The design starts at [`docs/concept.md`](docs/concept.md).
> What each version added is in [`CHANGELOG.md`](CHANGELOG.md).

## Numbers so far

Three machines. **AWS** is the one every release is measured on: an EC2
`m6id.xlarge` in Tokyo (four virtual CPUs, 16 GiB, Ubuntu 24.04, the logs
on its local NVMe disk), three runs for 0.1.0 on 2026-11-11, the median in
each cell. The others are for reference, 2026-11-03 and 2026-11-04: an
Apple M4 MacBook Air (32 GB, internal SSD, macOS), and a GitHub-hosted
Linux runner (Ubuntu 24.04, four virtual CPUs) shared with other work, over
two runs. Every write on both
sides is on disk before it is acknowledged (Valkey with `appendfsync
always`). Synthetic data, 32-byte values, 1,000,000 keys present.
Latencies are medians unless marked p99. Umbrachyx comes first in each
pair; an empty cell was not measured on that machine.

| | AWS (release machine) | Laptop (macOS) | Linux CI runner |
|---|---|---|---|
| `GET` while 50 clients write | **~18× faster** (0.031 vs 0.575 ms) | **~650× faster** (0.015 vs 9.86 ms) | **~20–28× faster** (0.031–0.039 vs 0.78–0.86 ms) |
| `GET` p99, 10,000 connections | **~1.6× faster** (98 vs 152 ms) | **not served** (macOS thread limit) | **~2× faster** (133 vs 279 ms) |
| `GET` p99, 5,000 connections | **~2.8× faster** (39 vs 110 ms) | **~5× slower** (170 vs 36 ms) | **~2.4× faster** (62 vs 147 ms) |
| `GET` p99, 1,000 connections | ~1.2× faster (9.3 vs 11.6 ms) | ~1.5× faster (6.1 vs 9.2 ms) | ~1.7× faster (10.5 vs 18.4 ms) |
| `GET`, one client | **~1.3× slower** (0.031 vs 0.023 ms) | ~3.5× faster (0.015 vs 0.055 ms) | the same (0.031 vs 0.031 ms) |
| `GET`s a second, 50 clients | **~1.9× fewer** (86,881 vs 165,563) | about the same (233,100 vs 218,818) | **~1.4–1.7× fewer** (66,138–73,314 vs 92,937–127,389) |
| One durable write | **~1.3× fewer a second** (5,055 vs 6,790) | the same (4.0 vs 4.0 ms): the disk's flush | **~1.4–1.5× fewer a second** (2,228–2,912 vs 3,139–4,351) |
| Durable writes, 256 writers at once | | ~1.3× more (30,773 vs 24,301 a second) | |
| Diff of 10 changes, vs Dolt | **~18× faster** (17 vs 320 µs, 1,000,000 records) | **~10× faster** (9 vs 96 µs, 10,000,000 records) | |
| Loading records, vs Dolt | **~6× faster** (1.1 vs 7 s, 1,000,000 records) | **~5× faster** (7.1 vs 36 s, 10,000,000 records) | |

The rows compare against Valkey unless they say Dolt. How much faster
depends on the machine. The first row is a real difference in design:
Valkey answers on one thread, after the sync of the writes around it,
while Umbrachyx reads a published snapshot that a write replaces by
swapping one pointer. So the gap is as wide as a sync is slow: about 4 ms
on the laptop, about 0.3 ms on the runner, about 0.13 ms on AWS. Over a socket, a `GET` alone
costs what a `PING` costs for both servers, so the one-client and
50-client rows compare how each server handles a connection, not how fast
it finds a value. Umbrachyx serves each connection on its own thread:
macOS caps a process at 8,192 threads, so 10,000 connections fail there,
while Linux serves them. Where Valkey is ahead on the runner (one writer,
50 readers), the same gaps show in a Linux virtual machine, and profiling
there points at the design rather than at a slip: after its sync, a write
in Umbrachyx still updates the history's tree and is handed between two
threads before it is answered, and with 50 readers much of the server's
time goes to walking the tree from node to node. The runner is noisy: its
two runs differ by up to a third. The 256-writer row measures Umbrachyx in
process and Valkey over loopback; the diff row includes Dolt's SQL layer.

Rough sizes from the AWS run, in memory, per key written once and per
later overwrite (`bench/memory.sh`, 200,000 keys):

| | A million keys, each written once | Each overwrite after that |
|---|---|---|
| Umbrachyx, history | about 2.4 GB | about 3.1 KB |
| Umbrachyx, latest-value table | about 0.7 GB | about 650 bytes |
| Valkey | about 55 MB | about 16 bytes |

Memory is where Umbrachyx loses most. Valkey keeps only the current value
of each key; Umbrachyx also keeps every commit (who, why, when), the
earlier values and the trees that make diffs and branches possible, which
is where the memory goes. Every SET is kept as a commit, so each one adds
about 3.1 KB, even when it overwrites a key; Valkey adds about 16 bytes.
400,000 SETs over 200,000 keys left Umbrachyx at 1.0 GiB and Valkey at
21 MiB (AWS, `bench/memory.sh`; a Linux virtual machine on the laptop
gave the same within a few percent). It was about 8 KB a SET until commits stopped keeping
a whole tree each; reading an old state now rebuilds its tree, a few
milliseconds. A table can instead be declared to keep only its latest
values: the same SETs then add about 650 bytes each, still about 40
times Valkey's, and leave it at 268 MiB; that table has no past to read
or undo. Over the server its GETs and SETs are about as fast as with
history: a little faster from one client, slower in the slowest reads of
50 clients at once. Nothing is dropped from a history yet; a retention policy is
designed but not built.

How each number was taken, and what it does not show:
[`docs/comparison.md`](docs/comparison.md). The raw output, machine and
versions: [`bench/results/`](bench/results/) (AWS in
`2026-11-11-aws-m6id.xlarge/`, the laptop in `2026-11-04/`,
the runner in `2026-11-04-linux-ci/`, the virtual machine in
`2026-11-09-linux-vm/`, `2026-11-09-linux-vm-sparse-trees/` and
`2026-11-09-linux-vm-node-hash/`, and with a latest table in
`2026-11-10-linux-vm-latest-table/` and, for speed on the laptop,
`2026-11-10-laptop-latest-table/`). The scripts are in
[`bench/`](bench/), and CI runs them against Valkey on every change.

## Robustness

| Test | Runs | Result |
|---|---|---|
| Kill the server with SIGKILL while 16 clients send `SET`s (half pipelined) | 100 | 330,912 acknowledged, none lost |
| Kill a process appending to the log | 1,000 | 3,719 acknowledged, none lost |
| Simulated disk: power cuts tearing writes at any sector, failed syncs, bit flips | 10,000 seeds | 130,677 restarts; every acknowledged record back, 6,867 corruptions refused |
| Simulated store: commits, branches, merges, reverts, approvals and rejections through crashes and a clock that goes back | 10,000 seeds | 596,644 commits, 151,846 restarts, 4,519 approvals, 10,535 rejections; every branch as acknowledged, every rejection kept |
| Simulated encrypted store: commits, erasures and failed key file writes through crashes | 10,000 seeds | 406,949 commits, 70,093 erasures, 120,624 restarts; every value as acknowledged, no erased value read again |

CI runs all of them on Linux and Windows for every change. The server test
and the simulations were also run against deliberately broken versions
(acknowledging before the write is durable, skipping a sync, misreading
damage) and caught each of them. On the AWS release machine for 0.1.0
they passed again, with 762,582 SETs acknowledged across the 100 server
kills and 80,423 writes across the 1,000 log kills, none lost
([`bench/results/2026-11-11-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-11-aws-m6id.xlarge/robustness.txt)).
Earlier output:
[`bench/results/2026-11-04/robustness.txt`](bench/results/2026-11-04/robustness.txt),
and for the encrypted store
[`bench/results/2026-11-05-sealed/robustness.txt`](bench/results/2026-11-05-sealed/robustness.txt).

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
designed so that they work against Umbrachyx as well. The adapter reads them
from the server, with the `UMBRA.` commands described in
[`docs/decisions.md`](docs/decisions.md).

## Working on it

CI runs `cargo fmt --check`, `cargo clippy` and `cargo test` on Linux and
Windows. To run the same three before each commit, enable the hook once per
clone:

```sh
git config core.hooksPath .githooks
```

Commits that touch no Rust or Cargo file skip the checks.

## License

[MIT](LICENSE)
