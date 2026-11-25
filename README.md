# Umbrachyx

**An in-memory database that keeps every change as history, where an AI
agent's writes wait on a branch until a person approves them.**

- **Every change is kept**, with who made it, why, and who approved it,
  to branch, diff, merge and undo: per key and per cell, the way git
  keeps files.
- **Reads never wait for writes.** While 50 clients write durably, a `GET`
  answers in tens of microseconds, about 27 times sooner than Valkey with
  every write synced, on the machine releases are measured on.
- **Agents write only to their own branch.** An agent's capability token
  names its branch; nothing it holds can write to `main` or approve a
  change. [`docs/agents.md`](docs/agents.md) lists what a hijacked agent
  was tested not to be able to do. The one exception is chosen table by
  table: a table declared to keep no history has no branches, and there
  the token decides only which keys.
- **A person sees a list of changes waiting, reads the diff, and approves
  or rejects.** What is approved is the diff that person saw: anything
  the agent wrote after it is refused, not merged unseen.
- **Nothing acknowledged is lost.** Killed mid-write 1,100 times with
  2,388,554 writes acknowledged on the release machine, and put through
  more than 580,000 simulated restarts after power cuts and disk faults:
  not one acknowledged write went missing. A table can be declared to
  answer before the disk instead (`plain-async`); then a failure loses
  its last second of writes and nothing else, and no other table is
  affected.

It is not a faster Valkey. In three runs on the release machine, with
the history kept, one client reads about 1.5 times as many values a
second as with Valkey and writes durably about 1.4 times as many, and 50
readers get as many. Many writers at once get 2.0 to 2.2 times fewer
writes through, writes sent 64 at a time about 7.4 times fewer, and
keeping history costs far more memory (about 655 bytes a write against
16). A table declared to keep no history is the kind of table Valkey
is: with reads in the load it does 1.6 to 2.0 times what Valkey does,
and with writes alone it is behind from 64 clients up, or on every row
when it answers before the disk as Valkey's default does. All of these
are in [the same tables](#numbers-so-far) as the rows where Umbrachyx is
ahead, measured on AWS and Google Cloud against Valkey, PostgreSQL and
Dolt, with the raw output published.

**Two yardsticks.** Operations a second, next to Valkey, is the one
experience with databases teaches a person to ask for, and the tables
below hold Umbrachyx to it. The other came from the AI agent this
project is built with: to an agent, speed is less how many operations a
second than how cheap one round of read, try and check is, and how cheap
a mistake is. The first release is measured by the first; the direction
after it is ordered by the second
([`docs/concept.md`](docs/concept.md), "Where it starts, and where it is
going").

The same in one page, in English and Japanese:
<https://meta-taro.github.io/umbrachyx/>.

## How a change gets in

```
agent ──writes──▶ its own branch ──waits──▶ a person: list ▶ diff ▶ approve ──▶ main
                                                              └──▶ reject (kept, with why)
```

1. **A person issues the agent a token** for one branch, some keys and
   columns, a purpose and an expiry (`umbrachyx token issue`). The agent
   reaches the database through MCP (`umbrachyx mcp`) or the Redis
   protocol with `AUTH <token>`.
2. **The agent commits to its branch**, each commit with a reason.
   Production (`main`) does not change.
3. **The person asks what is waiting.** `UMBRA.PENDING` lists each
   change: who wrote it and why, how many keys it touches in which
   table, and whether it would conflict with what `main` did meanwhile.
   `UMBRA.DIFF` shows it key by key.
4. **The person approves or rejects**, with a token that may approve and
   cannot write (`UMBRA.APPROVE`, `UMBRA.REJECT`). An approval is a
   merge into `main` that names the approver; a rejection is kept with
   its reason. A conflict merges nothing and names the keys.

A database viewer can do steps 3 and 4 as a screen:
[dbboard](https://github.com/meta-taro/dbboard) has a tab for it. From
the command line they are `umbrachyx diff` and `umbrachyx merge
--approved-by`, as in the quick start below and in
[`docs/examples/`](docs/examples/).

## Install

There are two ways today. There is no package for `dnf`, `apt` or
Homebrew and no prebuilt binary yet.

| Way | What it needs |
|---|---|
| **The container image**, `ghcr.io/meta-taro/umbrachyx` ([In a container](#in-a-container)) | Docker, on amd64 or arm64. It holds the server and the command-line tool. |
| **From source**, `cargo build --release` in a checkout ([Quick start](#quick-start)) | Rust through [rustup](https://rustup.rs), which fetches the version this repository pins (1.98.0); on Linux and macOS also a C compiler and `make`, for the memory allocator. The binary is `target/release/umbrachyx`. |

Where it has been run, and what was run there:

| System | What ran |
|---|---|
| Ubuntu 24.04 on x86-64 (AWS and Google Cloud) | Built from source; every benchmark and robustness test, at each release |
| Ubuntu on x86-64 (GitHub's runner) | Built from source; all tests and the robustness tests, on every change |
| Windows Server on x86-64 (GitHub's runner) | The same tests and robustness tests, on every change |
| macOS 26 on Apple silicon | Built from source and the tests run while developing; not in CI |
| The image (Debian 12) on amd64 and arm64 | Started, written to, killed, started again and read back, on every change |
| Fedora, RHEL and its relatives, other distributions | Not tried. The image should run wherever Docker does; nobody has built it from source there yet |

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
- **Not every table needs a past.** `umbrachyx table session plain --who
  you --why "sessions need no past"` declares one that keeps none: no
  commits, no branches, no review, each key just holds its current value,
  written with `SET` or `umbrachyx put session/1/token=abc`. Declared
  `plain-async` instead, a `SET` is also answered before it is on disk
  and written within a second, as Valkey does with `appendfsync
  everysec`. What each kind of table keeps and gives up is in
  [`docs/decisions.md`](docs/decisions.md#tables-that-keep-no-history-plain-and-plain-async).
- **`umbrachyx backup <directory>` copies the log and the keys as they
  are at one moment**, and `umbrachyx restore <directory>` writes a new
  store from the copy. Running it again adds what was written since;
  `umbrachyx backup <directory> --check` says whether the backup would
  restore, without restoring it. A
  subject erased since has no key in the backup afterwards; older copies
  of the backup still do, so keep them only as long as an erasure may
  take. `umbrachyx serve --backup-to <directory>` keeps the backup up
  to date while the server runs, without holding writes for the copy.
  Neither is in a released version yet: build from source to try them
  ([`docs/decisions.md`](docs/decisions.md#backup-and-restore)).
- **A second server can follow the first and answer reads.** Restore a
  backup on another machine and start it with `umbrachyx serve <branch>
  --follow <host:port>`; the first server is started with
  `--feed-replicas-on <port>`, a port to open to its replicas only. The replica asks for the records it lacks, holds
  the same commits under the same ids, and refuses writes with
  `READONLY`; `UMBRA.REPLICATION` says how far it is, and `umbrachyx
  replication --port <port>` prints the same for a running server and
  exits with 1 if its replication is not going well. Given a
  certificate, its key and an authority's certificate each
  (`--replication-cert`, `--replication-key`, `--replication-ca`, PEM
  files you make), the two servers speak TLS 1.3, answer nobody whose
  certificate that authority did not sign, and the replica is sent the
  keys of subjects first written after its backup. Without them nothing
  between the servers is encrypted and no key is sent: such a subject
  stops the replica, which says so. Started with
  `--current-values-only` as well, a replica keeps in memory only the
  values as they are now: after two million SETs over a million keys it
  held 193 MB where one that keeps the history held 1,432 MB (one
  laptop, both servers on it), and it answers questions about the past
  with `NOHISTORY`; its log on disk is whole all the same. Still for
  trials: a replica far behind can
  still be stopped for good by erasures it missed (16 MiB or more
  between a value and its erasure, or an erased subject written again
  next to one erased for good), and
  nothing is measured on two machines yet. Started with
  `--wait-for-replica <seconds>`, the first server answers a write only
  once a replica holds it; killed in the middle of writes, every write
  it acknowledged was on the replica (100 rounds, 72,964 writes, both
  servers on one laptop; over TLS with every write to a new subject,
  100 rounds, 23,596 writes). Not in a released version: build from source to try it
  ([`docs/decisions.md`](docs/decisions.md#replication-being-built)).
- **`umbrachyx export` prints the whole log as JSON Lines**, one record a
  line, to search with `jq` or `grep`:
  `umbrachyx export | jq 'select(.who == "agent:support")'`.
- **On `main`, a merge is the change.** It is credited to whoever merged;
  the agent's own commit, with its reason, stays on its branch.
- Here the agent is played by hand with `--who`. A real agent gets a token
  for its branch and talks to `umbrachyx mcp`; [`docs/agents.md`](docs/agents.md)
  sets one up. To watch the same history in a viewer, `umbrachyx serve
  <branch>` speaks the Redis protocol with `UMBRA.*` commands for history.
- **[`docs/examples/`](docs/examples/) has three small systems built this
  way**, each run by the tests as written: a game's inventory, a support
  desk, and settings rolled out in stages.
- **A later version opens a log this one wrote**, from 0.2.0 on. The key
  file kept beside the log, which values encrypted per subject need, and
  the `UMBRA.*` commands may still change.

### In a container

The server alone, with its log on a volume. Each version's image is
published for amd64 and arm64 (`ghcr.io/meta-taro/umbrachyx:0.2.0`;
without a version, the newest); `docker build -t umbrachyx .` in a
checkout builds the same from source.

```console
$ docker run -d --name umbrachyx -p 127.0.0.1:6380:6380 -v umbrachyx-data:/data ghcr.io/meta-taro/umbrachyx
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

> **Status: 0.2.0, the first released version.** A single-node core, so the design can be
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
> [`docs/comparison.md`](docs/comparison.md). A later version opens the
> log this one wrote; the key file and the `UMBRA.*` commands may still
> change. It is one node; a second server can follow it and
> answer reads, for trials only (below): try
> it and measure it, and do not make it the only copy of data you need.
> The design starts at [`docs/concept.md`](docs/concept.md).
> What each version added is in [`CHANGELOG.md`](CHANGELOG.md).

## Numbers so far

One machine, the one every release is measured on: an EC2 `m6id.xlarge`
in Tokyo (four virtual CPUs, 16 GiB, Ubuntu 24.04, the logs on its local
NVMe disk), three runs on 2026-11-17 at commit `20b1673`, the median in
each cell. Synthetic data, 32-byte values, 1,000,000 keys present.

First, tables that keep their history, which is what a table does unless
it is declared otherwise. Every write on every side is on disk before it
is acknowledged (Valkey with `appendfsync always`, PostgreSQL as it
ships). PostgreSQL reads and updates one row by its primary key with
prepared statements; it keeps no history, and is here as the database
without one. Valkey keeps only the current value too.

| A second, unless it says otherwise | Umbrachyx, with history | Valkey | PostgreSQL | Against Valkey |
|---|---|---|---|---|
| `GET` while 50 clients write: median / p99 | **0.023 / 0.47 ms** | 0.62 / 1.01 ms | 0.21 / 0.88 ms | **~27× sooner** |
| `GET`s while 50 clients write | **24,667** | 1,758 | 3,748 | **~14× more** |
| `GET` p99 with 10,000 connections | **107 ms** | 182 ms | | **~1.7× sooner** |
| `GET` p99 with 5,000 connections | **52 ms** | 113 ms | | **~2.1× sooner** |
| `GET` p99 with 1,000 connections | **9.4 ms** | 15.0 ms | | **~1.6× sooner** |
| `GET`s, one client | **54,377** | 37,216 | 27,307 | **~1.5× more** |
| Durable writes, one client | 8,832 | 6,268 | **10,203** | **~1.4× more** |
| Durable writes, 8 clients | 26,706 | 24,555 | **32,176** | 9% more |
| `GET`s, 50 clients | 149,477 | **151,745** | 67,849 | 1% fewer (143,472 to 156,495) |
| `GET`s, 50 clients sending 16 at a time | 860,585 | **1,201,923** | | **~1.4× fewer** |
| Durable writes, 64 clients | 43,649 | **94,473** | 40,274 | **~2.2× fewer** |
| Durable writes, 256 clients | 56,577 | **112,360** | 28,148 | **~2.0× fewer** |
| Durable writes, 50 clients sending 64 at a time | 84,221 | **626,959** | | **~7.4× fewer** |
| The same on a server with fewer keys, without and with a capability token | 124,177 / 42,670 | | | |
| Diff of 10 changes in two commits, vs Dolt | **1.1 µs** | | | Dolt: 309 µs (1,000,000 records) |
| Loading 1,000,000 records, vs Dolt | **1.5 s** | | | Dolt: 7 s |

Where Umbrachyx is ahead, the design is why. Valkey answers on one
thread, after the sync of the writes around it; Umbrachyx reads a
published snapshot that a write replaces by swapping one pointer, on
every serving thread at once, so a read never waits for a write and many
connections share the processors. One client's write is one sync on both
sides, and the log is zeroed ahead of its records so that the sync has
only the record to make durable.

Where it is behind, one thread is the reason. Every commit is applied by
a single thread: it reads the cell the write replaces, copies the path
of the tree to it, hashes the commit and checks the token. With a
million keys and their history held, the rows where sync is not the
limit top out near 57,000 writes a second from separate clients and
84,000 sent 64 at a time, however many clients send them; Valkey appends
to a buffer and syncs once for all of them. A capability token costs
each write a further check. The 50-client `GET` row is not steady: a run
gives less when the benchmark's client lands on the core a serving
thread is on
([`bench/results/2026-11-13-aws-m6id.xlarge-next/`](bench/results/2026-11-13-aws-m6id.xlarge-next/)),
which is why its range is beside its median.

Second, tables declared to keep no history, which is the kind of table
Valkey is. A `plain` table still syncs every write before it answers,
and is set against Valkey with `appendfsync always`; a `plain-async`
table answers first and syncs within a second, and is set against
Valkey's default, `appendfsync everysec`. The history column is the
table above again, for the price of the history.

| Writes a second | With history | Plain | Valkey `always` | Plain-async | Valkey `everysec` |
|---|---|---|---|---|---|
| One client | 8,832 | **10,680** | 6,268 | 29,061 | **34,554** |
| 8 clients | 26,706 | **46,436** | 24,555 | 107,585 | **151,745** |
| 64 clients | 43,649 | 90,050 | **94,473** | 106,838 | **156,495** |
| 256 clients | 56,577 | 107,181 | **112,360** | 98,377 | **119,261** |
| 50 clients sending 64 at a time | 84,221 | 540,687 | **626,959** | 572,082 | **638,570** |

| Reads and writes mixed, 50 clients, operations a second | With history | Plain | Valkey `always` | Plain-async | Valkey `everysec` |
|---|---|---|---|---|---|
| 95% reads | 138,039 | **162,179** | 81,269 | **170,792** | 106,042 |
| 80% reads | 114,288 | **149,576** | 77,656 | **170,350** | 103,999 |
| 50% reads | 67,301 | **124,003** | 72,454 | **156,246** | 100,182 |

Writes alone, a plain table is ahead of Valkey from one and eight
clients, 5% behind from 64 and 256, and 14% behind sent 64 at a time; a
plain-async table is behind Valkey's `everysec` on every row, by 10 to
32%. A write to it crosses two threads before it is answered, and
Valkey's crosses none. With reads mixed in it is the other way round: a plain table does 1.7 to 2.0
times what Valkey does with `always`, a plain-async table 1.6 times
what it does with `everysec`, and a table that keeps its history is
ahead of `always` too until half the operations are writes (7% fewer
there). The mixed rows are sent by one program for both servers
(`bench/mixed.sh`), one command at a time from each of 50 clients.

Starting again after five million writes takes 57.5 s when the keys
keep their history and 3.3 s when they are in a plain table: the log
keeps every write of both (655 MB and 455 MB), and the history is what
takes the time to rebuild. Nothing shortens a log yet.

The second machine a release is measured on, a Google Cloud
`n2-standard-4` with a slower disk, gives lower numbers and the same
picture: the same rows ahead and the same rows behind, by more where it
is behind (a plain table's writes from 256 clients 27% fewer than
Valkey's, a plain-async table's 19 to 46% fewer than `everysec`)
([`bench/results/2026-11-17-gcp-n2-standard-4/`](bench/results/2026-11-17-gcp-n2-standard-4/)).
Two instances of one machine type differ too: the run of the day before
on another `m6id.xlarge` gave up to a third more on some rows from
almost the same code, and two commits measured in turn on a third
instance showed the instance, not the code, to be the difference
([`bench/results/2026-11-17-aws-m6id.xlarge-ab/`](bench/results/2026-11-17-aws-m6id.xlarge-ab/)).
A factor between two servers holds on one machine; a number alone does
not carry over to the next.

Next to noisy neighbours on the same machine (other programs keeping
every processor busy, syncing files on the same disk, churning 70% of
the memory and talking over the network; `bench/colocated.sh`), the
server kept answering, at about a seventh of its rate: 1,208 durable
writes a second from one client instead of 9,368, and 21,988 `GET`s from
50 clients instead of 157,520, with the slowest percent of those `GET`s
at 10 ms. Killed 100 times mid-write beside them, it lost none of 518,966
acknowledged writes. Given 1 GiB of memory and filled until it could get
no more, it stopped saying `memory allocation of 36480 bytes failed`,
and all 342,912 writes it had acknowledged were there when it started
again ([`colocated.txt`](bench/results/2026-11-17-aws-m6id.xlarge/colocated.txt)).

In memory, per key written once and per later overwrite
(`bench/memory.sh`, 200,000 keys):

| | A million keys, each written once | Each overwrite after that |
|---|---|---|
| Umbrachyx, history | about 0.8 GB | about 655 bytes |
| Umbrachyx, latest-value table on a branch | about 0.7 GB | about 600 bytes |
| Umbrachyx, plain table | about 0.13 GB | about 48 bytes |
| Valkey | about 55 MB | about 16 bytes |

Memory is still where a table with history is furthest behind: about 40
times Valkey's per overwrite, 14 times per key. Valkey keeps only the
current value of each key; Umbrachyx keeps every commit (who, why, when)
and the values it replaced. A plain table keeps neither and comes to
about 3 times Valkey's per overwrite and 2.3 times per key. With
history, an overwrite was about 3.1 KB when first measured, when one commit in
1,024 kept a whole tree; now only the head of each branch has
one, and reading the whole state of an old commit rebuilds it from the
nearer end of its history ([`docs/decisions.md`](docs/decisions.md), "The
store"). A latest-value table keeps no past on `main`; on any other
branch it keeps the cells the branch wrote, for merges, which is why it
saves nothing here, where the server writes to a branch. Nothing is
dropped from a history yet; a retention policy is designed but not built.

The same server as first measured (2026-11-11), on this machine, a laptop and a Linux
CI runner, with how each number was taken and what it does not show:
[`docs/comparison.md`](docs/comparison.md). Since then, on this
machine: one client's durable write went from 1.3 times fewer than
Valkey's to 1.4 times more, one client's `GET` from 1.3 times slower to
ahead by half, 50 clients' `GET`s from 1.9 times fewer to level, and an
overwrite from about 3.1 KB to about 655 bytes. The raw output, machine and
versions: [`bench/results/`](bench/results/) (this run in
`2026-11-17-aws-m6id.xlarge/`, the ones before in
`2026-11-16-aws-m6id.xlarge/`, `2026-11-14-aws-m6id.xlarge/` and
`2026-11-12-aws-m6id.xlarge/`, the first in `2026-11-11-aws-m6id.xlarge/`).
The scripts are in [`bench/`](bench/), and CI runs them against Valkey
on every change.

## Robustness

| Test | Runs | Result |
|---|---|---|
| Kill the server with SIGKILL while 16 clients send `SET`s (half pipelined) | 100 | 2,312,686 acknowledged, none lost |
| Kill a process appending to the log | 1,000 | 75,868 acknowledged, none lost |
| Simulated disk: power cuts tearing writes at any sector, failed syncs, bit flips | 10,000 seeds | 130,306 restarts; every acknowledged record back, 6,647 corruptions and 198 damaged file headers refused |
| Simulated store: commits, branches, merges, reverts, approvals and rejections through crashes and a clock that goes back, with a second store following its log | 10,000 seeds | 596,193 commits, 152,028 restarts, 4,536 approvals, 10,530 rejections; every branch as acknowledged, every rejection kept; the follower never held a record the store did not, and ended with all 459,601 |
| Simulated encrypted store: commits, erasures and failed key file writes through crashes | 10,000 seeds | 406,973 commits, 70,090 erasures, 120,619 restarts; every value as acknowledged, no erased value read again |
| Simulated tables without history: puts answered before they are on disk and writes that wait for it, through failed syncs, crashes and power cuts, with a second store following the log | 10,000 seeds | 432,389 groups of puts answered early, 346,893 writes that waited, 180,718 restarts; 78,351 of them lost the last puts answered early and never anything else |

CI runs all of them on Linux and Windows for every change. The server test
and the simulations were also run against deliberately broken versions
(acknowledging before the write is durable, skipping a sync, misreading
damage) and caught each of them. The table is the run on the AWS release
machine on 2026-11-17
([`bench/results/2026-11-17-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-17-aws-m6id.xlarge/robustness.txt));
all of it passed the same day on the Google Cloud machine too.
Earlier output: the runs before in
[`bench/results/2026-11-14-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-14-aws-m6id.xlarge/robustness.txt) and
[`bench/results/2026-11-12-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-12-aws-m6id.xlarge/robustness.txt),
the first on the same machine in
[`bench/results/2026-11-11-aws-m6id.xlarge/robustness.txt`](bench/results/2026-11-11-aws-m6id.xlarge/robustness.txt),
[`bench/results/2026-11-04/robustness.txt`](bench/results/2026-11-04/robustness.txt),
and for the encrypted store
[`bench/results/2026-11-05-sealed/robustness.txt`](bench/results/2026-11-05-sealed/robustness.txt).

Not tested yet: power cuts on real hardware (only simulated), runs longer
than minutes, and more than one machine: a replica following its
primary is tested on one machine and in simulation, and not measured.
A restart replays the whole log: 57.5 s after five million writes to
tables with history, 3.3 s after as many to a plain table.

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
repositories. dbboard connects to `umbrachyx serve` as it would to a Redis
server and reads the history with the `UMBRA.` commands described in
[`docs/decisions.md`](docs/decisions.md): a key's history, a diff between
branches or commits, and the list of changes waiting, with approve and
reject. Nothing in those commands is specific to dbboard; any client that
speaks the Redis protocol can send them.

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
