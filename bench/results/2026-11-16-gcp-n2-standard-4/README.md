# Results of 2026-11-16 on Google Cloud: the second release machine

The first run on the second of the two fixed machines a release is
measured on: `bench/cloud.sh` three times, the robustness tests and the
server next to noisy neighbours, at the commit measured on AWS the same
day ([`../2026-11-16-aws-m6id.xlarge/`](../2026-11-16-aws-m6id.xlarge/)).

## Machine

- Compute Engine `n2-standard-4` in `asia-northeast1-b` (Tokyo), on
  demand: 4 virtual CPUs (Intel Cascade Lake, 2.8 GHz), 16 GB of memory,
  Ubuntu 24.04.5 with Linux 7.0 (GCP kernel). Transparent huge pages
  were at Ubuntu's default, `madvise`.
- The logs, Valkey's append-only file, PostgreSQL's data directory and
  Dolt's data were on one 375 GB local SSD (NVMe, ext4), not on a
  network disk.
- Rust 1.98.0, Valkey 9.1.3 built from its release tag, PostgreSQL 16.15
  from Ubuntu's package, Dolt 2.6.2, set up as on AWS.
- Synthetic data only. `machine.txt` has the rest.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/` … `run-3/` | `bench/cloud.sh gcp-n2-standard-4 <ssd dir> 3` | 60c6584 |
| `robustness.txt` | the three robustness tests, from the same script | 60c6584 |
| `colocated.txt` | `bench/colocated.sh <ssd dir>`, from the same script | 60c6584 |
| `commit-cost.txt` | the `commit_cost` example with a million keys, as built and with `_RJEM_MALLOC_CONF=thp:default,metadata_thp:disabled`, three rounds in turn, after the script had finished | 60c6584 |

The files in each `run-N/` are the ones described in the AWS folder.

## What stands out

Medians of the three runs, Valkey with `appendfsync always`.

| A second, unless it says otherwise | Umbrachyx | Valkey | PostgreSQL |
|---|---|---|---|
| `GET` while 50 clients write: median / p99 | 0.031 / 0.41 ms | 1.18 / 2.66 ms | 0.076 / 0.90 ms |
| `GET`s while 50 clients write | 18,671 | 880 | 6,668 |
| `GET` p99 with 1,000 / 5,000 / 10,000 connections | 15 / 77 / 134 ms | 22 / 149 / 274 ms | |
| `GET`s, one client | 42,017 | 31,437 | 23,199 |
| Durable writes, one client | 3,653 | 2,887 | 4,517 |
| `GET`s, 50 clients | 99,108 | 103,306 | 49,550 |
| Durable writes, 8 clients | 11,787 | 11,365 | 17,482 |
| `GET`s, 50 clients sending 16 at a time | 722,543 | 948,317 | |
| Durable writes, 64 clients | 24,190 | 46,915 | 24,241 |
| Durable writes, 256 clients | 27,439 | 73,883 | 20,280 |
| Durable writes, 50 clients sending 64 at a time | 69,813 | 460,829 | |
| The same on a server with fewer keys, without and with a capability token | 108,184 / 39,374 | | |

- **The order of the rows is the one on AWS.** Ahead of Valkey: reads
  while others write, many connections, one client's reads and writes,
  and narrowly eight clients' writes. Level within what the row varies
  by: 50 clients' `GET`s, 4% fewer at the median, the three runs 99,108,
  76,046 and 104,384. Behind: 1.3 times fewer `GET`s sent 16 at a time,
  1.9 and 2.7 times fewer writes from 64 and 256 clients, 6.6 times
  fewer sent 64 at a time.
- **This disk syncs more slowly than the AWS one**, which every durable
  row on every side shows: one client's durable write is 3,653 a second
  here and 10,045 there, Valkey's 2,887 and 7,066.
- **Huge pages take about a fifth off the store's own work**
  (`commit-cost.txt`): a commit costs 17.4 to 17.8 µs written one at a
  time and 17.2 to 17.7 µs written 64 to a sync, against 21.4 to 22.3
  and 22.0 to 23.4 µs with small pages. Reading a snapshot does not
  change (0.15 µs).
- **Memory per overwrite is about 650 bytes** (644 to 663), Valkey's 17.
- A diff of 10 changes in two commits takes 1.27 µs, Dolt's 427 µs;
  loading a million records 1.9 s, Dolt's 10 s.
- The server crash test acknowledged 1,488,274 SETs across its 100
  kills and lost none (940 s); the log crash test 31,225 across 1,000;
  the three simulations of 10,000 seeds passed.

## Next to noisy neighbours (`colocated.txt`)

| A second | Alone | Crowded |
|---|---|---|
| Durable writes, one client | 3,392 | 750 |
| `GET`s, one client | 45,921 | 6,241 |
| Durable writes, 50 clients | 27,116 | 6,657 |
| `GET`s, 50 clients | 101,285 | 13,421 |

The server answered throughout, at about a quarter to an eighth of its
rate. Killed 100 times mid-write beside the same neighbours, it lost
none of 378,264 acknowledged writes. Allowed 1 GiB of address space and
filled until it could get no more, it stopped with `memory allocation of
38926 bytes failed` (exit status 134), and all 342,784 writes it had
acknowledged were there when it started again.

## How the run went

The instance ran from 02:47 to 04:37 UTC, created with a three-hour
limit after which it deletes itself. The measurement started eight
minutes after it was up: the time it took a person to confirm the host
key and hand over the terminal. The three runs took about 55 minutes,
the robustness tests 20 and the run next to neighbours 20. The results
were collected as one file whose SHA-256 was checked on both sides.
