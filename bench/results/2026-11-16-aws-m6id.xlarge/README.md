# Results of 2026-11-16 on AWS: the release machine, with huge pages

The fourth run on the fixed machine that each release is measured on:
`bench/cloud.sh` three times, the robustness tests and the server next
to noisy neighbours. The one change since the run before
([`../2026-11-14-aws-m6id.xlarge/`](../2026-11-14-aws-m6id.xlarge/)) is
that the server asks Linux for huge pages. The same commit was measured
the same day on Google Cloud:
[`../2026-11-16-gcp-n2-standard-4/`](../2026-11-16-gcp-n2-standard-4/).

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
  `valkey-benchmark` is the client for Umbrachyx and Valkey.
- PostgreSQL 16.15 from Ubuntu's package, a fresh server per run with
  `shared_buffers` 1 GB and otherwise as shipped; `pgbench` with prepared
  statements is its client.
- Dolt 2.6.2 from its release archive.
- Synthetic data only. `machine.txt` has the rest.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/` … `run-3/` | `bench/cloud.sh aws-m6id.xlarge <nvme dir> 3` | 60c6584 |
| `robustness.txt` | the three robustness tests, from the same script | 60c6584 |
| `colocated.txt` | `bench/colocated.sh <nvme dir>`, from the same script | 60c6584 |

In each `run-N/`: `umbrachyx.csv`, `valkey.csv` and `postgres.csv`
(`bench/umbrachyx.sh`, `bench/valkey.sh`, `bench/postgres.sh`; an empty
`postgres.stderr.txt` says that script had no errors), `connections.csv`
(`bench/connections.sh`), `memory.csv` (`bench/memory.sh`), `compare.md`
and `dolt.csv` (the compare example and `bench/dolt.sh` over the same
million records).

## What stands out

Medians of the three runs, Valkey with `appendfsync always`; the run
before in brackets.

| A second | Umbrachyx | Valkey |
|---|---|---|
| `GET`s, one client | 59,773 (54,230) | 40,128 (36,443) |
| Durable writes, one client | 10,045 (8,548) | 7,066 (6,401) |
| `GET`s, 50 clients | 163,399 (144,509) | 162,075 (153,374) |
| Durable writes, 8 clients | 31,017 (23,688) | 25,661 (24,963) |
| Durable writes, 64 clients | 51,773 (36,010) | 103,093 (98,717) |
| Durable writes, 256 clients | 60,680 (39,746) | 122,775 (116,347) |
| `GET`s, 50 clients sending 16 at a time | 1,166,181 (841,043) | 1,278,772 (1,218,769) |
| Durable writes, 50 clients sending 64 at a time | 109,951 (73,443) | 645,370 (629,327) |
| `GET`s while 50 clients write | 25,025 (24,612) | 1,885 (1,782) |

- **Writes from many clients rose by 40 to 50%**: more than the 16 to
  22% the comparison of two days before measured for huge pages alone
  on another instance of this machine type. This instance was also the
  faster of the two: Valkey's own rows, which did not change, are 3 to
  10% higher. The rest was not taken apart here; the Google Cloud run
  has the store's cost per commit with and without huge pages on one
  machine.
- **Eight clients' durable writes are now ahead of Valkey's**, 31,017
  against 25,661, and **50 clients' `GET`s are level**, 163,399 against
  162,075 (the three runs 166,389, 162,338 and 163,399).
- **Many writers at once are still behind**: 2.0 times fewer from 64
  clients and from 256, and 5.9 times fewer sent 64 at a time (2.7, 2.9
  and 8.6 in the run before).
- **A capability token costs writes almost three quarters**: 165,536 a
  second without one, 44,007 with one, on a third server with fewer
  keys. Huge pages helped the row without the token and not the check.
- **Memory per overwrite is about 660 bytes** (646 to 667), Valkey's 16.
  An empty server holds about 22 MiB, Valkey 7.
- **With 1,000, 5,000 and 10,000 connections** the slowest percent of
  `GET`s is 7.8, 40 and 85 ms, Valkey's 11, 68 and 138.
- PostgreSQL: 29,548 `GET`s and 11,116 updates a second from one client,
  74,048 `GET`s from 50, and 34,107, 43,493 and 39,777 updates from 8, 64
  and 256 clients.
- A diff of 10 changes in two commits takes 1.10 µs, Dolt's 295 µs;
  loading a million records 1.3 s, Dolt's 6 s.
- The server crash test acknowledged 2,509,805 SETs across its 100
  kills and lost none (1,213 s); the log crash test 82,180 across 1,000;
  the three simulations of 10,000 seeds passed.

## Next to noisy neighbours (`colocated.txt`)

| A second | Alone | Crowded |
|---|---|---|
| Durable writes, one client | 10,119 | 1,250 |
| `GET`s, one client | 61,952 | 5,806 |
| Durable writes, 50 clients | 65,012 | 12,361 |
| `GET`s, 50 clients | 169,753 | 23,974 |

The server answered throughout, at a fifth to a tenth of its rate.
Killed 100 times mid-write beside the same neighbours, it lost none of
573,588 acknowledged writes. Allowed 1 GiB of address space and filled
until it could get no more, it stopped with `memory allocation of 72960
bytes failed` (exit status 134), and all 342,784 writes it had
acknowledged were there when it started again.

## How the run went

The instance ran from 02:16 to 03:56 UTC. The measurement started six
minutes after it was up: the time it took a person to confirm the host
key and hand over the terminal. The three runs took about 50 minutes,
the robustness tests 26 and the run next to neighbours 22. The results
were read file by file and each file's size checked against the
machine's.
