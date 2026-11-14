# Results of 2026-11-14 on AWS: the release machine

The third run on the fixed machine that each release is measured on:
`bench/cloud.sh` three times, the robustness tests, and for the first
time the server next to noisy neighbours. These are the numbers in the
README. The run before is in
[`../2026-11-12-aws-m6id.xlarge/`](../2026-11-12-aws-m6id.xlarge/).

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
| `run-1/` … `run-3/` | `bench/cloud.sh aws-m6id.xlarge <nvme dir> 3` | 269dc7a |
| `robustness.txt` | the three robustness tests, from the same script | 269dc7a |
| `colocated.txt` | `bench/colocated.sh <nvme dir>`, from the same script | 269dc7a |
| `huge-pages.txt` | `huge-pages.sh`, after the script had finished | 269dc7a |

In each `run-N/`: `umbrachyx.csv`, `valkey.csv` and `postgres.csv`
(`bench/umbrachyx.sh`, `bench/valkey.sh`, `bench/postgres.sh`; an empty
`postgres.stderr.txt` says that script had no errors), `connections.csv`
(`bench/connections.sh`), `memory.csv` (`bench/memory.sh`), `compare.md`
and `dolt.csv` (the compare example and `bench/dolt.sh` over the same
million records).

## What changed since the run before

Between c6dd1b7 and 269dc7a: a leaf of the tree shares its keys with the
leaf it replaces; half the processors serve connections, each staying
awake a moment before it sleeps; the `aes` crate is held at 0.9.2; the
log and the key file begin with a header that names their format; the
server can listen on an address other than this machine's own. The
script gained the run next to noisy neighbours.

## What stands out

Medians of the three runs, Valkey with `appendfsync always`.

- **One client is ahead of Valkey**: 54,230 `GET`s a second against
  36,443, and 8,548 durable writes against 6,401. In the run before it
  was 35,829 against 37,495 and 7,506 against 6,476.
- **50 clients' `GET`s are 6% behind at the median**: 144,509 against
  153,374, the three runs being 160,256, 138,122 and 144,509 against a
  steady 151,515 to 153,846. The run before gave 101,317 against 156,495.
- **Many writers at once are still far behind**, by less: 36,010 a
  second from 64 clients against 98,717, 39,746 from 256 against
  116,347, and 73,443 against 629,327 when 50 clients send 64 at a time
  (27,488, 27,952 and 41,847 in the run before).
- **A read while 50 clients write** takes 0.023 ms at the median against
  Valkey's 0.61, and 24,612 of them get through a second against 1,782.
- **A capability token costs writes more than half**: 113,129 a second
  without one, 41,075 with one, on a third server with fewer keys.
- **Memory per overwrite is about 650 bytes**, 620 in the run before,
  Valkey's 16.
- **With 5,000 and 10,000 connections** the slowest percent of `GET`s is
  49 and 107 ms (31 and 74 in the run before), Valkey's 78 and 191.
- The server crash test acknowledged 2,258,381 SETs across its 100
  kills and took 1,165 s.

## Next to noisy neighbours (`colocated.txt`)

`stress-ng` kept all four processors busy, had two writers syncing files
on the same disk, two programs churning 35% of the memory each and two
talking over the network.

| A second | Alone | Crowded |
|---|---|---|
| Durable writes, one client | 9,362 | 1,331 |
| `GET`s, one client | 57,055 | 7,005 |
| Durable writes, 50 clients | 55,563 | 10,616 |
| `GET`s, 50 clients | 158,089 | 24,233 |

The server answered throughout, at about a seventh of its rate; the
slowest percent of the 50 clients' `GET`s took 9 ms. Killed 100 times
mid-write beside the same neighbours, it lost none of 471,378
acknowledged writes. Allowed 1 GiB of address space and filled until it
could get no more, it stopped with `memory allocation of 36480 bytes
failed` (exit status 134), and all 343,232 writes it had acknowledged
were there when it started again.

## Huge pages, in turn (`huge-pages.txt`)

Not a release number: a comparison of the allocator's defaults against
`_RJEM_MALLOC_CONF=thp:always,metadata_thp:always`, three rounds, one
after the other, with a million keys. The kernel's setting was
`madvise`.

| | Defaults | Huge pages |
|---|---|---|
| The store's own work per commit, one at a time | 14.4 to 14.7 µs | 11.1 to 11.3 µs |
| The same, 64 commits to a sync | 14.3 to 14.6 µs | 9.4 to 9.6 µs |
| Filling: writes sent 64 at a time, a second | 72,669 to 73,009 | 84,246 to 84,467 |
| Writing over them again | 66,591 to 67,604 | 81,288 to 82,386 |
| Durable writes, one client | 8,518 to 8,571 | 9,146 to 9,214 |
| `GET`s, one client | 54,201 to 54,900 | 55,772 to 56,101 |
| `GET`s, 50 clients | 156,177 to 159,693 | 145,836 to 160,927 |
| Memory held afterwards | 1,391 MiB | 1,407 to 1,410 MiB |

Writes gain 16 to 22% sent many at a time and about 7% from one client,
reads do not change beyond what that row varies by, and memory is 1.3%
more. A first version of `huge-pages.sh` measured the wrong process and
was stopped; the file is the second run, of the script as it is here.

## How the run went

The instance ran from 14:43 to 16:41 UTC. The measurement started two
minutes after it was up. The three runs took about 52 minutes, the
robustness tests 25, the run next to neighbours 27 and the huge-page
comparison 15.
