# Results of 2026-11-17 on AWS: the release machine, with tables that keep no history

The fifth run on the fixed machine that each release is measured on:
`bench/cloud.sh` three times, the robustness tests, a restart after five
million writes and the server next to noisy neighbours. New since the
run before ([`../2026-11-16-aws-m6id.xlarge/`](../2026-11-16-aws-m6id.xlarge/)):
`plain` and `plain-async` tables, Valkey with `appendfsync everysec`
beside them, reads and writes mixed, and the restart. The same commit
was measured the same day on Google Cloud:
[`../2026-11-17-gcp-n2-standard-4/`](../2026-11-17-gcp-n2-standard-4/).

## Machine

- Amazon EC2 `m6id.xlarge` in `ap-northeast-1` (Tokyo), on demand: 4
  virtual CPUs (Intel Xeon Platinum 8375C), 16 GiB of memory, Ubuntu
  24.04.5 with Linux 7.0 (AWS kernel). Transparent huge pages were at
  Ubuntu's default, `madvise`.
- The logs, Valkey's append-only file, PostgreSQL's data directory and
  Dolt's data were on the machine's local NVMe instance store (ext4), not
  on a network disk.
- Rust 1.98.0, the version the repository pins, built with `--release`.
- Valkey 9.1.3 built from its release tag (jemalloc). Its
  `valkey-benchmark` is the client for Umbrachyx and Valkey, except in
  `mixed.csv`, where one program of this repository
  (`examples/mixed_load.rs`) is the client of both.
- PostgreSQL 16.15 from Ubuntu's package, a fresh server per run with
  `shared_buffers` 1 GB and otherwise as shipped; `pgbench` with prepared
  statements is its client.
- Dolt 2.6.2 from its release archive.
- Synthetic data only. `machine.txt` has the rest.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/` … `run-3/` | `bench/cloud.sh aws-m6id.xlarge <nvme dir> 3` | 20b1673 |
| `robustness.txt` | the three robustness tests, from the same script | 20b1673 |
| `restart.csv` | `bench/restart.sh`, from the same script, once | 20b1673 |
| `colocated.txt` | `bench/colocated.sh <nvme dir>`, from the same script | 20b1673 |

In each `run-N/`: `umbrachyx.csv`, `valkey.csv` and `postgres.csv`
(`bench/umbrachyx.sh`, `bench/valkey.sh`, `bench/postgres.sh`; an empty
`postgres.stderr.txt` says that script had no errors), `connections.csv`
(`bench/connections.sh`), `memory.csv` (`bench/memory.sh`), `mixed.csv`
(`bench/mixed.sh`), `compare.md` and `dolt.csv` (the compare example and
`bench/dolt.sh` over the same million records).

## What stands out

Medians of the three runs. A table with history and a `plain` table
sync every write before they answer, as Valkey does with `appendfsync
always`; a `plain-async` table answers first and syncs within a second,
as Valkey does with `appendfsync everysec`. Each kind is set against the
Valkey setting that makes the same promise.

| Writes a second | History | Plain | Valkey `always` | Plain-async | Valkey `everysec` |
|---|---|---|---|---|---|
| One client | 8,832 | 10,680 | 6,268 | 29,061 | 34,554 |
| 8 clients | 26,706 | 46,436 | 24,555 | 107,585 | 151,745 |
| 64 clients | 43,649 | 90,050 | 94,473 | 106,838 | 156,495 |
| 256 clients | 56,577 | 107,181 | 112,360 | 98,377 | 119,261 |
| 50 clients sending 64 at a time | 84,221 | 540,687 | 626,959 | 572,082 | 638,570 |

| Reads and writes mixed, 50 clients, operations a second | History | Plain | Valkey `always` | Plain-async | Valkey `everysec` |
|---|---|---|---|---|---|
| 95% reads | 138,039 | 162,179 | 81,269 | 170,792 | 106,042 |
| 80% reads | 114,288 | 149,576 | 77,656 | 170,350 | 103,999 |
| 50% reads | 67,301 | 124,003 | 72,454 | 156,246 | 100,182 |

- **A plain table writes 2.1 times what a table with history does from
  64 clients and 6.4 times sent 64 at a time**, and is level with
  Valkey's `always` from 64 and 256 clients (5% fewer), ahead from one
  and 8 (1.7 and 1.9 times), and 14% behind sent 64 at a time.
- **A plain-async table is behind Valkey's `everysec` on every row of
  writes alone**: 16% fewer from one client, 29% from 8, 32% from 64,
  18% from 256 and 10% sent 64 at a time.
- **With reads mixed in, every kind of table is ahead of the Valkey
  setting it is set against**, except a table with history at 50%
  reads (7% fewer than `always`): plain 2.0, 1.9 and 1.7 times
  `always`, plain-async 1.6 times `everysec` at each ratio. Valkey
  answers reads and writes on one thread; here the reads are spread over
  the serving threads.
- **`GET`s while 50 clients write**: 24,667 a second with history,
  19,818 from a plain table and 32,531 from a plain-async one; Valkey
  with `always` 1,758. `bench/valkey.sh` has no such row for `everysec`.
- **A restart after five million writes** (`restart.csv`) takes 57.5 s
  when the keys keep their history (a 655 MB log) and 3.3 s when they
  are in a plain table (455 MB): the log keeps every write of both, and
  only the history is rebuilt in memory.
- **Memory per overwrite**: 656 bytes with history (655 to 657), 600 in
  a latest-value table on a branch (600 to 631), 48 in a plain table (47
  to 49), Valkey's 16 (15 to 17). After 200,000 keys written once a
  server holds 167 MiB with history, 47 MiB with a plain table, Valkey
  18 MiB.
- **With 1,000, 5,000 and 10,000 connections** the slowest percent of
  `GET`s is 9.4, 52 and 107 ms, Valkey's 15, 113 and 182.
- **A capability token** costs writes two thirds: 124,177 a second
  without one, 42,670 with one, on a third server with fewer keys.
- PostgreSQL: 27,307 `GET`s and 10,203 updates a second from one client,
  67,849 `GET`s from 50, and 32,176, 40,274 and 28,148 updates from 8, 64
  and 256 clients.
- A diff of 10 changes in two commits takes 1.12 µs, Dolt's 309 µs;
  loading a million records 1.5 s, Dolt's 7 s.
- The server crash test acknowledged 2,312,686 SETs across its 100
  kills and lost none (1,157 s); the log crash test 75,868 across 1,000;
  the four simulations of 10,000 seeds passed, the one of plain tables
  among them: of its 180,718 restarts, 78,351 lost the last puts a
  plain-async table had answered and never anything else.

## Against the run of the day before

The rows that were measured both days are lower here: with history,
one client's durable writes 8,832 (10,045), 64 clients' 43,649
(51,773), 256 clients' 56,577 (60,680), writes sent 64 at a time 84,221
(109,951), `GET`s sent 16 at a time 860,585 (1,166,181). Valkey's rows
fell by 3 to 11%, Umbrachyx's by up to 26%. On Google Cloud the same
day the same rows rose: Umbrachyx's by up to 29%, Valkey's by 2 to 11%
but for one that fell.

To tell the machine from the code, the commit of the day before and
this one were measured in turn on a third instance of this type
([`../2026-11-17-aws-m6id.xlarge-ab/`](../2026-11-17-aws-m6id.xlarge-ab/)):
they are within 3% of each other on every row but one that one round
disturbed, and both give what this run gave, not what the day before
gave. The instance of the day before was the faster one; this is not
the code.

## Next to noisy neighbours (`colocated.txt`)

| A second | Alone | Crowded |
|---|---|---|
| Durable writes, one client | 9,368 | 1,208 |
| `GET`s, one client | 57,084 | 11,947 |
| Durable writes, 50 clients | 57,246 | 10,985 |
| `GET`s, 50 clients | 157,520 | 21,988 |

The server answered throughout, at a fifth to an eighth of its rate.
Killed 100 times mid-write beside the same neighbours, it lost none of
518,966 acknowledged writes. Allowed 1 GiB of address space and filled
until it could get no more, it stopped with `memory allocation of 36480
bytes failed` (exit status 134), and all 342,912 writes it had
acknowledged were there when it started again.

## How the run went

The instance ran from 02:07 to 03:55 UTC. The measurement started six
minutes after it was up: a person confirmed the host key and handed
over the terminal, and the first command to fetch the code had the
address of the repository wrong. The three runs took about 55 minutes,
the robustness tests 25, the restart 3 and the run next to neighbours
about 20. The results were read file by file and each file's size checked
against the machine's.
