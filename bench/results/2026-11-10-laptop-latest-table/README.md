# Results of 2026-11-10 on the laptop: GET and SET in a latest table

Three runs of `bench/umbrachyx.sh`, which now measures the same GETs and
SETs twice: on a server whose keys keep their history, and on a fresh one
whose keys are in a table declared to keep only its latest values. They
are behind the "A table that keeps only latest values" section of
[`docs/comparison.md`](../../../docs/comparison.md).

## Machine

- Apple M4 MacBook Air (fanless, 32 GB), macOS 26.6.2, internal SSD with
  APFS, on AC power. Nothing else heavy running.
- Rust 1.98.0 (the version the repository pins), built with `--release`.
- Valkey 9.1.3 from Homebrew; only its `valkey-benchmark` client is used
  here. Valkey itself was not run again.
- Synthetic data only.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/`, `run-2/`, `run-3/` `umbrachyx.csv` | `bench/umbrachyx.sh` | the commit that adds this folder |

Rows whose first column says `latest table` are the second server. Its
keys are `pos/bench/key:<n>`, 1,000,000 of them written before the GETs,
as the first server's `key:<n>` are; they share one subject, as
`key:<n>` do, so neither server makes an encryption key per key. Both
servers write to a branch. Latencies are in milliseconds, in the coarse
buckets `valkey-benchmark` reports.
