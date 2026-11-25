# Results of 2026-11-25 on a laptop: who holds a put to a plain-async table

Not a release run. One laptop, two builds in turn, to see what it
changes that the thread which reads a `SET` to a plain-async table seals
its value and holds the put itself, where it handed it to the one
thread that writes and was woken with the answer.

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2, its own SSD. Rust 1.98.0,
`valkey-benchmark` 9.1.3 as the client, on the same machine. Four
serving threads (the default here). Synthetic data only, 32-byte values,
a million keys.

## Files

| File | What | Code |
|---|---|---|
| `ab.csv` | `bench/ab.sh <old> <new> 3 plain-async` | old `4573dc3`, new the commit that adds this folder |

## What it says

Writes a second, lowest to highest of the three rounds:

| Clients | Old | New |
|---|---|---|
| 1 | 42,499 to 42,589 | 63,091 to 64,998 |
| 8 | 143,575 to 146,413 | 219,780 to 223,464 |
| 64 | 206,612 to 208,768 | 240,964 to 242,424 |
| 256 | 206,186 to 208,768 | 238,095 to 242,131 |
| 50, sending 64 at a time | 993,542 to 1,000,000 | 1,200,480 to 1,205,546 |

The client runs on the same machine and takes processors of its own, so
what a server on a machine of its own would give is not what this says.
Valkey was not measured in this run.
