# Umbrachyx

**An in-memory database that keeps every change as history.**

- **Every change is kept**, with who made it and why, to branch, diff,
  merge and undo: per key and per cell, the way git keeps files.
- **Agents write only to their own branch.** An agent's capability token
  names its branch; nothing it holds can write to `main`. A person reads
  the diff and merges it. [`docs/agents.md`](docs/agents.md) lists what a
  hijacked agent was tested not to be able to do.
- **Reads never wait for writes.** While 50 clients write durably, a `GET`
  answers in tens of microseconds, about 19 times sooner than Valkey with
  every write synced, on the machine releases are measured on.
- **Nothing acknowledged is lost.** Killed mid-write 1,100 times with
  2,202,833 writes acknowledged on the release machine, and put through
  more than 400,000 simulated restarts after power cuts and disk faults:
  not one acknowledged write went missing.

It is not a faster Valkey. One client reads and writes about as fast as
with Valkey, but 50 readers get about 1.5 times fewer answers, many
writers at once 3.6 to 4.2 times fewer, writes sent 64 at a time 15 times
fewer, and keeping history costs far more memory (about 620 bytes a
write against 16). All of these are in
[the same table](#numbers-so-far) as the rows where Umbrachyx is ahead,
measured on AWS against Valkey, PostgreSQL and Dolt, with the raw output
published.

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
- **`umbrachyx export` prints the whole log as JSON Lines**, one record a
  line, to search with `jq` or `grep`:
  `umbrachyx export | jq 'select(.who == "agent:support")'`.
- **On `main`, a merge is the change.** It is credited to whoever merged;
  the agent's own commit, with its reason, stays on its branch.
- Here the agent is played by hand with `--who`. A real agent gets a token
  for its branch and talks to `umbrachyx mcp`; [`docs/agents.md`](docs/agents.md)
  sets one up. To watch the same history in a viewer, `umbrachyx serve
  <branch>` speaks the Redis protocol with `UMBRA.*` commands for history.
- **No format is stable yet.** A later version may not open a log this
  one wrote; keep the commands that load your data, not the log.

### In a container

The server alone, with its log on a volume. An image is published with
the first released version; until then, build it from a checkout:

```console
$ docker build -t umbrachyx .
$ docker run -d --name umbrachyx -p 127.0.0.1:6380:6380 -v umbrachyx-data:/data umbrachyx
$ valkey-cli -p 6380 set player/42/gems 100
OK
$ valkey-cli -p 6380 umbra.history app player/42/gems
{"branch":"app","changes":[{"approved_by":[],"commit":"0bc77a4dff61","erased":false,"time_us":1794570631599977,"value":"100","who":"unauthenticated","why":"SET"}],"key":"player/42/gems","more":false}
```

- **Publish the port to `127.0.0.1` only, as above.** Whoever reaches the
  port reads every value and its history without a token, and nothing on
  the connection is encrypted. `-p 6380:6380` would open it to the
  network the machine is on.
- **The volume holds the log and, next to it, the keys.** Keep both
  together: without its keys a log cannot be read. A container killed
  mid-write loses nothing it acknowledged; the next one on the same
  volume starts from the log.
- **Clients write to a branch**, `app` unless `-e UMBRACHYX_BRANCH=<name>`
  says otherwise, made from `main` on the first start.
- **Options for `serve` go after the image**, for instance
  `umbrachyx serve --public-key ed25519/… --require-auth` to take writes
  only from connections that `AUTH` with a token.

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

One machine, the one every release is measured on: an EC2 `m6id.xlarge`
in Tokyo (four virtual CPUs, 16 GiB, Ubuntu 24.04, the logs on its local
NVMe disk), three runs on 2026-11-12 at commit `c6dd1b7`, the median in
each cell. Every write on every side is on disk before it is acknowledged
(Valkey with `appendfsync always`, PostgreSQL as it ships). Synthetic
data, 32-byte values, 1,000,000 keys present. PostgreSQL reads and
updates one row by its primary key with prepared statements; it keeps no
history, and is here as the database without one.

| A second, unless it says otherwise | Umbrachyx | Valkey | PostgreSQL | Against Valkey |
|---|---|---|---|---|
| `GET` while 50 clients write: median / p99 | **0.031 / 0.34 ms** | 0.60 / 0.98 ms | 0.19 / 0.84 ms | **~19× sooner** |
| `GET`s while 50 clients write | **22,946** | 1,811 | 3,987 | **~12.7× more** |
| `GET` p99 with 10,000 connections | **74 ms** | 215 ms | | **~2.9× sooner** |
| `GET` p99 with 5,000 connections | **31 ms** | 78 ms | | **~2.5× sooner** |
| `GET` p99 with 1,000 connections | **7.8 ms** | 14.5 ms | | **~1.9× sooner** |
| Durable writes, one client | **7,506** | 6,476 | 10,691 | **~1.16× more** |
| `GET`s, one client | 35,829 | 37,495 | 27,694 | about the same (5% fewer) |
| `GET`s, 50 clients | 101,317 | **156,495** | 70,048 | **~1.5× fewer** |
| `GET`s, 50 clients sending 16 at a time | 771,010 | **1,239,157** | | **~1.6× fewer** |
| Durable writes, 8 clients | 19,212 | 25,013 | **33,163** | **~1.3× fewer** |
| Durable writes, 64 clients | 27,488 | **98,232** | 41,546 | **~3.6× fewer** |
| Durable writes, 256 clients | 27,952 | **117,165** | 29,059 | **~4.2× fewer** |
| Durable writes, 50 clients sending 64 at a time | 41,847 | **638,570** | | **~15× fewer** |
| The same, from connections that sent a capability token | 32,979 | | | |
| Diff of 10 changes in two commits, vs Dolt | **1.0 µs** | | | Dolt: 302 µs (1,000,000 records) |
| Loading 1,000,000 records, vs Dolt | **1.3 s** | | | Dolt: 7 s |

Where Umbrachyx is ahead, the design is why. Valkey answers on one
thread, after the sync of the writes around it; Umbrachyx reads a
published snapshot that a write replaces by swapping one pointer, on
every serving thread at once, so a read never waits for a write and many
connections share the processors. One client's write is one sync on both
sides, and the log is zeroed ahead of its records so that the sync has
only the record to make durable.

Where it is behind, one thread is the likely reason. Every commit is
applied by a single thread: it reads the cell the write replaces, copies
the path of the tree to it, hashes the commit and checks the token; a
profile in a Linux virtual machine shows that thread busy while the
others wait ([`docs/decisions.md`](docs/decisions.md), "The store"). On
this machine the rows where sync is not the limit top out near 42,000
writes a second, about 24 µs a commit with a million keys and their
history held, however many clients send them; Valkey appends to a buffer
and syncs once for all of them. A
capability token costs each write a further check. With 50 readers, each
`GET` crosses a lock and walks the tree, where Valkey looks up a hash
table. These are the next things to change, and the table is measured
again when they have.

Two of them have changed since the table, and it has not been measured
again as a whole yet. A leaf of the tree now shares its keys with the
leaf it replaces, and half the processors serve connections, each
staying awake a moment before it sleeps. One run of each on the same
machine on 2026-11-13, beside Valkey that day
([`bench/results/2026-11-13-aws-m6id.xlarge-ab/`](bench/results/2026-11-13-aws-m6id.xlarge-ab/)):

| A second | Umbrachyx | Valkey |
|---|---|---|
| `GET`s, one client | **54,466** | 35,186 |
| Durable writes, one client | **8,813** | 6,737 |
| `GET`s, 50 clients | 156,740 | 161,290 |
| Durable writes, 8 clients | 23,480 | **25,602** |
| Durable writes, 64 clients | 35,689 | **95,329** |
| Durable writes, 256 clients | 38,292 | **117,855** |
| Durable writes, 50 clients sending 64 at a time | 72,908 | **611,434** |
| `GET`s, 50 clients sending 16 at a time | 677,048 | **1,247,661** |

Since that run, holding the `aes` crate at 0.9.2 took `GET`s sent 16 at
a time from about 650,000 to about 825,000 a second on the same machine
([`bench/results/2026-11-13-aws-m6id.xlarge-aes/`](bench/results/2026-11-13-aws-m6id.xlarge-aes/)).

The 50-client `GET` row is not steady: about one run in five gives
110,000 to 145,000, when the benchmark's client lands on the core a
serving thread is on
([`bench/results/2026-11-13-aws-m6id.xlarge-next/`](bench/results/2026-11-13-aws-m6id.xlarge-next/)). With 5,000 and 10,000 connections the slowest percent
of `GET`s is now 52 and 120 ms, not the 31 and 74 ms of the table, and
memory is about 4% more.

In memory, per key written once and per later overwrite
(`bench/memory.sh`, 200,000 keys):

| | A million keys, each written once | Each overwrite after that |
|---|---|---|
| Umbrachyx, history | about 0.7 GB | about 620 bytes |
| Umbrachyx, latest-value table on a branch | about 0.7 GB | about 625 bytes |
| Valkey | about 55 MB | about 16 bytes |

Memory is still where Umbrachyx is furthest behind: about 37 times
Valkey's per overwrite, 12 times per key. Valkey keeps only the current
value of each key; Umbrachyx keeps every commit (who, why, when) and the
values it replaced. It was about 3.1 KB an overwrite in 0.1.0, when one
commit in 1,024 kept a whole tree; now only the head of each branch has
one, and reading the whole state of an old commit rebuilds it from the
nearer end of its history ([`docs/decisions.md`](docs/decisions.md), "The
store"). A latest-value table keeps no past on `main`; on any other
branch it keeps the cells the branch wrote, for merges, which is why it
saves nothing here, where the server writes to a branch. Nothing is
dropped from a history yet; a retention policy is designed but not built.

The same server measured as 0.1.0 on this machine, a laptop and a Linux
CI runner, with how each number was taken and what it does not show:
[`docs/comparison.md`](docs/comparison.md). Since 0.1.0, on this
machine: one client's durable write went from 1.3 times fewer than
Valkey's to 1.16 times more, one client's `GET` from 1.3 times slower to
level, 50 clients' `GET`s from 1.9 to 1.5 times fewer, and an overwrite
from about 3.1 KB to about 620 bytes. The raw output, machine and
versions: [`bench/results/`](bench/results/) (this run in
`2026-11-12-aws-m6id.xlarge/`, 0.1.0 in `2026-11-11-aws-m6id.xlarge/`).
The scripts are in [`bench/`](bench/), and CI runs them against Valkey
on every change.

## Robustness

| Test | Runs | Result |
|---|---|---|
| Kill the server with SIGKILL while 16 clients send `SET`s (half pipelined) | 100 | 2,124,844 acknowledged, none lost |
| Kill a process appending to the log | 1,000 | 77,989 acknowledged, none lost |
| Simulated disk: power cuts tearing writes at any sector, failed syncs, bit flips | 10,000 seeds | 131,346 restarts; every acknowledged record back, 6,809 corruptions refused |
| Simulated store: commits, branches, merges, reverts, approvals and rejections through crashes and a clock that goes back, with a second store following its log | 10,000 seeds | 596,644 commits, 151,846 restarts, 4,519 approvals, 10,535 rejections; every branch as acknowledged, every rejection kept; the follower never held a record the store did not, and ended with all 459,779 |
| Simulated encrypted store: commits, erasures and failed key file writes through crashes | 10,000 seeds | 406,949 commits, 70,093 erasures, 120,624 restarts; every value as acknowledged, no erased value read again |

CI runs all of them on Linux and Windows for every change. The server test
and the simulations were also run against deliberately broken versions
(acknowledging before the write is durable, skipping a sync, misreading
damage) and caught each of them. The table is the run on the AWS release
machine on 2026-11-12
([`bench/results/2026-11-12-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-12-aws-m6id.xlarge/robustness.txt)).
Earlier output: 0.1.0 on the same machine in
[`bench/results/2026-11-11-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-11-aws-m6id.xlarge/robustness.txt),
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
