# Results of 2026-11-11 on AWS: the release machine for 0.1.0

The first run on the fixed machine that each release is measured on:
`bench/cloud.sh` three times, then the robustness tests, then the diff
comparison again with a fix to how it is timed. These are the numbers in
the README's first column and in [`docs/comparison.md`](../../../docs/comparison.md).

## Machine

- Amazon EC2 `m6id.xlarge` in `ap-northeast-1` (Tokyo), on demand: 4
  virtual CPUs (Intel Xeon Platinum 8375C), 16 GiB of memory, Ubuntu 24.04.5
  with Linux 7.0 (AWS kernel).
- The logs, Valkey's append-only file and Dolt's data were on the
  machine's local NVMe instance store (ext4), not on a network disk.
- Rust 1.98.0, the version the repository pins, built with `--release`.
- Valkey 9.1.3 built from its release tag (jemalloc). Its
  `valkey-benchmark` is the client for both servers.
- Dolt 2.6.2 from its release archive.
- Synthetic data only. `machine.txt` has the rest.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/` … `run-3/` | `bench/cloud.sh aws-m6id.xlarge <nvme dir> 3` | 15a5341 |
| `robustness.txt` | the three robustness tests, from the same script | 15a5341 |
| `compare-3063727/run-1.md` … `run-3.md` | `cargo run --release -p umbrachyx --example compare -- 1000000`, three times | 3063727 |

In each `run-N/`: `umbrachyx.csv` and `valkey.csv` (`bench/umbrachyx.sh`,
`bench/valkey.sh`), `connections.csv` (`bench/connections.sh`; its stderr
says every connection got a thread), `memory.csv` (`bench/memory.sh`),
`compare.md` and `dolt.csv` (the compare example and `bench/dolt.sh` over
the same million records).

## Why the diff was run again

`run-N/compare.md` prints the diff of ten changes as `0` µs. A commit
compared with its parent is read from the cells it wrote, without walking
either tree, and that took about half a microsecond, which the example
rounded to zero. Dolt's `dolt_diff` walks two trees, so that column is not
the one to set beside it. 3063727 adds the same ten changes made in two
commits, which makes the diff walk both trees, and prints both with two
decimals. Only the example changed between 15a5341 and 3063727 that
affects these numbers; the store and the tree are the same code.

## What stands out

- A sync on this disk takes about 0.13 ms, against about 4 ms on the
  laptop. Everything that waits on a sync is faster here, Valkey's too, so
  the gap in `GET` while 50 clients write is about 18 times rather than
  650.
- The server crash test took 1,127 s here against about 500 s on the
  laptop: with fast syncs each round acknowledges more writes (762,582 in
  all), and each restart reads a longer log back.
