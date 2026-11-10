# Results of 2026-11-10 on the laptop: GETs from 50 clients, after SETs

Three runs of `bench/umbrachyx.sh`, which now runs the latest table's
`SET` from one client before its `GET`s, as it already did for the
history. They replace the latest-table rows of
[`2026-11-10-laptop-latest-table/`](../2026-11-10-laptop-latest-table/)
in the "A table that keeps only latest values" section of
[`docs/comparison.md`](../../../docs/comparison.md).

In that earlier folder, the latest table's `GET` from 50 clients came
straight after a long run of one client's `SET`s, which mostly wait on
the disk, and its slowest reads were 3.6 times the history's. Run after
the `GET` from one client instead, as the history's are, they are about
the same as the history's.

`read_contention.txt` is `examples/read_contention.rs` on the same code:
GETs from 10 and from 50 threads on one view, in process, with no network,
three runs each (`1000000 <threads> 200000`). Without the network a read
takes 1–2 µs, and there the latest table's slowest reads are about twice
the history's, from the lock taken around the encryption keys on every
read. Over the server a read takes about 110 µs and the difference does
not show.

## Machine

- Apple M4 MacBook Air (fanless, 10 cores, 32 GB), macOS 26.6.2, internal
  SSD with APFS, on AC power. Nothing else heavy running.
- Rust 1.98.0 (the version the repository pins), built with `--release`.
- Valkey 9.1.3 from Homebrew; only its `valkey-benchmark` client is used
  here. Valkey itself was not run again.
- Synthetic data only.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/`, `run-2/`, `run-3/` `umbrachyx.csv` | `bench/umbrachyx.sh` | the commit that adds this folder |
| `read_contention.txt` | `cargo run --release --example read_contention -- 1000000 <threads> 200000` | the commit that adds this folder |

Rows whose first column says `latest table` are the second server, with
keys `pos/bench/key:<n>` under one subject, as in the earlier folder.
Latencies are in milliseconds in the CSVs, in the coarse buckets
`valkey-benchmark` reports, and in nanoseconds in `read_contention.txt`.
