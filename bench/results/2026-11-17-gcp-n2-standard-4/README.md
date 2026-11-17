# Results of 2026-11-17 on Google Cloud: the second release machine, with tables that keep no history

The second run on the second of the two fixed machines a release is
measured on: `bench/cloud.sh` three times, the robustness tests, a
restart after five million writes and the server next to noisy
neighbours, at the commit measured on AWS the same day
([`../2026-11-17-aws-m6id.xlarge/`](../2026-11-17-aws-m6id.xlarge/)).
New since the run before
([`../2026-11-16-gcp-n2-standard-4/`](../2026-11-16-gcp-n2-standard-4/)):
`plain` and `plain-async` tables, Valkey with `appendfsync everysec`
beside them, reads and writes mixed, and the restart.

## Machine

- Compute Engine `n2-standard-4` in `asia-northeast1-b` (Tokyo), on
  demand: 4 virtual CPUs (Intel Xeon, 2.8 GHz), 16 GB of memory, Ubuntu
  24.04.5 with Linux 7.0 (GCP kernel). Transparent huge pages were at
  Ubuntu's default, `madvise`.
- The logs, Valkey's append-only file, PostgreSQL's data directory and
  Dolt's data were on one 375 GB local SSD (NVMe, ext4), not on a
  network disk.
- Rust 1.98.0, Valkey 9.1.3 built from its release tag, PostgreSQL 16.15
  from Ubuntu's package, Dolt 2.6.2, set up as on AWS.
- Synthetic data only. `machine.txt` has the rest.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/` … `run-3/` | `bench/cloud.sh gcp-n2-standard-4 <ssd dir> 3` | 20b1673 |
| `robustness.txt` | the three robustness tests, from the same script | 20b1673 |
| `restart.csv` | `bench/restart.sh`, from the same script, once | 20b1673 |
| `colocated.txt` | `bench/colocated.sh <ssd dir>`, from the same script | 20b1673 |

The files in each `run-N/` are the ones described in the AWS folder.

## What stands out

Medians of the three runs. A table with history and a `plain` table
sync every write before they answer, as Valkey does with `appendfsync
always`; a `plain-async` table answers first and syncs within a second,
as Valkey does with `appendfsync everysec`.

| Writes a second | History | Plain | Valkey `always` | Plain-async | Valkey `everysec` |
|---|---|---|---|---|---|
| One client | 4,103 | 4,752 | 3,036 | 16,351 | 27,375 |
| 8 clients | 13,870 | 21,678 | 11,910 | 62,247 | 109,051 |
| 64 clients | 28,337 | 47,450 | 50,994 | 59,102 | 108,519 |
| 256 clients | 35,342 | 58,275 | 80,225 | 58,634 | 90,171 |
| 50 clients sending 64 at a time | 77,244 | 458,085 | 498,256 | 467,727 | 574,053 |

| Reads and writes mixed, 50 clients, operations a second | History | Plain | Valkey `always` | Plain-async | Valkey `everysec` |
|---|---|---|---|---|---|
| 95% reads | 100,363 | 121,578 | 54,059 | 124,869 | 81,475 |
| 80% reads | 77,121 | 100,452 | 51,059 | 120,262 | 80,026 |
| 50% reads | 46,773 | 75,775 | 48,439 | 113,274 | 75,835 |

- **The same rows are ahead and behind as on AWS, and it is further
  behind where it is behind.** A plain table is ahead of Valkey's
  `always` from one and 8 clients (1.6 and 1.8 times), 7% behind from
  64, 27% from 256 and 8% sent 64 at a time. A plain-async table is
  behind `everysec` on every row of writes alone: 40% fewer from one
  client, 43% from 8, 46% from 64, 35% from 256 and 19% sent 64 at a
  time.
- **A plain table writes 1.7 times what a table with history does from
  64 clients and 5.9 times sent 64 at a time.**
- **With reads mixed in, a plain table does 2.2, 2.0 and 1.6 times what
  Valkey does with `always`, and a plain-async table 1.5 times what it
  does with `everysec`** at each ratio. A table with history is ahead
  of `always` at 95% and 80% reads (1.9 and 1.5 times) and 3% behind at
  50%.
- **`GET`s while 50 clients write**: 20,367 a second with history,
  17,892 from a plain table and 19,992 from a plain-async one; Valkey
  with `always` 948.
- **A restart after five million writes** takes 61.5 s when the keys
  keep their history and 3.4 s when they are in a plain table.
- **Memory per overwrite**: 657 bytes with history (656 to 677), 600 in
  a latest-value table on a branch (600 to 611), 45 in a plain table
  (24, 45 and 46 in the three runs), Valkey's 17.
- **With 1,000, 5,000 and 10,000 connections** the slowest percent of
  `GET`s is 13, 73 and 136 ms, Valkey's 16, 138 and 238.
- **A capability token** costs writes two thirds: 128,833 a second
  without one, 40,195 with one, on a third server with fewer keys.
- PostgreSQL: 25,055 `GET`s and 4,811 updates a second from one client,
  53,066 `GET`s from 50, and 18,395, 28,417 and 25,402 updates from 8, 64
  and 256 clients.
- A diff of 10 changes in two commits takes 1.29 µs, Dolt's 416 µs;
  loading a million records 1.8 s, Dolt's 10 s.
- The server crash test acknowledged 1,601,256 SETs across its 100
  kills and lost none (956 s); the log crash test 35,889 across 1,000;
  the four simulations of 10,000 seeds passed.

## Against the run of the day before

The rows that were measured both days are higher here, Umbrachyx's by up
to 29% and Valkey's by 2 to 11% (one Valkey row without persistence fell
by 14%): with history, one client's durable writes 4,103 (3,653), 64
clients' 28,337 (24,190), 256 clients' 35,342 (27,439), writes sent 64
at a time 77,244 (69,813). On AWS the same day they were lower. Two
commits measured in turn on one machine
([`../2026-11-17-aws-m6id.xlarge-ab/`](../2026-11-17-aws-m6id.xlarge-ab/))
differ by 3% at most: the instances differ, not the code.

## Next to noisy neighbours (`colocated.txt`)

| A second | Alone | Crowded |
|---|---|---|
| Durable writes, one client | 4,348 | 948 |
| `GET`s, one client | 48,918 | 19,485 |
| Durable writes, 50 clients | 32,807 | 9,116 |
| `GET`s, 50 clients | 103,536 | 17,409 |

The server answered throughout, at two fifths to a sixth of its rate.
Killed 100 times mid-write beside the same neighbours, it lost none of
418,505 acknowledged writes. Allowed 1 GiB of address space and filled
until it could get no more, it stopped with `memory allocation of 39296
bytes failed` (exit status 134), and all 343,872 writes it had
acknowledged were there when it started again.

## How the run went

The instance ran from 02:14 to 04:24 UTC. The measurement that is here
started 27 minutes after it was up. Five minutes went to a person
confirming the host key and handing over the terminal. The first start
then stopped at once: the Ubuntu mirror of the region answered 404 for
two packages, and that was only noticed 20 minutes later, when the
output was next read. Installing the packages again worked, and the
script was started again from its beginning. The three runs took about
57 minutes, the robustness tests 20, the restart 3 and the run next to
neighbours 20. The results were read file by file and each file's size
checked against the machine's.
