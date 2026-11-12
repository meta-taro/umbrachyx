# Results of 2026-11-12 on AWS: the release machine, after 0.1.0

The second run on the fixed machine that each release is measured on:
`bench/cloud.sh` three times, then the robustness tests. These are the
numbers in the README. The first run, of 0.1.0, is in
[`../2026-11-11-aws-m6id.xlarge/`](../2026-11-11-aws-m6id.xlarge/).

## Machine

- Amazon EC2 `m6id.xlarge` in `ap-northeast-1` (Tokyo), on demand: 4
  virtual CPUs (Intel Xeon Platinum 8375C), 16 GiB of memory, Ubuntu
  24.04.5 with Linux 7.0 (AWS kernel).
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
| `run-1/` … `run-3/` | `bench/cloud.sh aws-m6id.xlarge <nvme dir> 3` | c6dd1b7 |
| `robustness.txt` | the three robustness tests, from the same script | c6dd1b7 |

In each `run-N/`: `umbrachyx.csv`, `valkey.csv` and `postgres.csv`
(`bench/umbrachyx.sh`, `bench/valkey.sh`, `bench/postgres.sh`; an empty
`postgres.stderr.txt` says that script had no errors), `connections.csv`
(`bench/connections.sh`), `memory.csv` (`bench/memory.sh`), `compare.md`
and `dolt.csv` (the compare example and `bench/dolt.sh` over the same
million records).

## What changed since the run before

Between 15a5341 and c6dd1b7: the log is zeroed ahead of its records;
writers write their own groups instead of handing them to a thread; a
few threads serve all connections; a token's signatures are checked once
per connection; no commit keeps a whole tree; diffs between commits on
one line are read from the cells between them. The scripts gained rows:
writes from 8, 64 and 256 clients, reads and writes sent many at a time,
writes with and without a capability token, and PostgreSQL.

## What stands out

- **One client is level with Valkey or ahead**: 7,506 durable writes a
  second against 6,476, and 35,829 `GET`s against 37,495. In the run
  before it was 5,055 against 6,790 and 0.031 ms against 0.023 ms.
- **Many writers at once are far behind**: 27,488 a second from 64
  clients against Valkey's 98,232, and 41,847 against 638,570 when 50
  clients send 64 at a time. These rows were not measured before. The
  rate stops near 42,000 a second whatever the clients, about 24 µs a
  commit: more than twice what the same rows gave in a virtual machine
  on a laptop, with a million keys and their history held.
- **A capability token costs writes more than half**: 84,685 a second
  without one, 32,979 with one, on a third server with fewer keys.
- **Memory per overwrite is a fifth of what it was**: about 620 bytes
  against about 3.1 KB, Valkey's 16.
- **PostgreSQL**, which keeps no history, is ahead for writes from 1 to
  64 clients and behind for every read.
- The server crash test acknowledged 2,124,844 SETs across its 100
  kills, against 762,582 in the run before, and took 1,085 s.

## How the run went

The instance ran from 02:15 to 04:01 UTC. For about 28 minutes of that
it did nothing: the run could not start until a person had confirmed the
host and handed over the console, and the request for that was written
where it was not read. The measurements took about 39 minutes for the
three runs and 24 for the robustness tests.
