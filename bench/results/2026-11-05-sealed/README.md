# Results of 2026-11-05: values encrypted

One run of `bench/umbrachyx.sh` after values began to be encrypted under
a key per subject (AES-SIV), on the same laptop and with the same script
as [`../2026-11-04/`](../2026-11-04/), to see what encryption costs. It
is a before-and-after check of Umbrachyx against itself, not a comparison
with Valkey, and not yet reflected in the top-level README.

## Machine

The same as on 2026-11-04: Apple M4 MacBook Air (fanless, 32 GB), macOS
26.6.2, internal SSD with APFS, on AC power; Rust 1.98.0, `--release`;
Valkey 9.1.3's `valkey-benchmark` as the client. Synthetic data only.

## Files

| File | Script | Code |
|---|---|---|
| `umbrachyx.csv` | `bench/umbrachyx.sh` | the commit that adds this folder |
| `robustness.txt` | the 10,000-seed simulations, command at its top | the commit that adds this file |

`robustness.txt` adds the simulation of the encrypted store, with its
key file, erasures and failed key file writes, to the two simulations
run before. The first two lines match 2026-11-04's exactly: the same
seeds replay the same runs.

The script's standard error was captured into the same file this time;
one line the shell printed when the script stopped its background
writers (`Terminated: 15`) was removed. Nothing else was changed.

## Against 2026-11-04

| Row | 2026-11-04 | Encrypted |
|---|---|---|
| Durable `SET`s a second, one client | 241 | 217 |
| `GET`s a second, one client | 58,106 | 58,309 |
| `GET`s a second, 50 clients | 233,100 (p99 0.175 ms) | 189,753 (p99 0.375 ms) |
| `GET` while 50 clients write | 74,738 a second | 72,046 a second |

Reading with 50 clients is about a fifth slower, and a durable write about
a tenth; the rest is within what one run on a laptop varies by. Every read
now opens its value, which includes expanding the subject's AES key each
time; keeping expanded keys is the first thing to try. Writes to keys with
no subject, as `valkey-benchmark` makes, share one key, so the extra sync
a new subject costs does not appear here.
