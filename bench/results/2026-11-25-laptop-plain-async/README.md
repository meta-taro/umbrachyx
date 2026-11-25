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
| `ab.csv` | `bench/ab.sh <old> <new> 3 plain-async` | old `a9ed3fc`, new the commit that adds this folder |

The new build also seals a value with its key set up once, which the
old one does not
([`../2026-11-25-laptop-siv/`](../2026-11-25-laptop-siv/)): the two
changes are not told apart here.

## What it says

Writes a second, lowest to highest of the three rounds:

| Clients | Old | New |
|---|---|---|
| 1 | 41,434 to 41,736 | 64,704 to 65,104 |
| 8 | 140,647 to 142,755 | 221,976 to 223,964 |
| 64 | 203,874 to 206,825 | 234,192 to 245,399 |
| 256 | 208,986 to 210,084 | 225,989 to 243,013 |
| 50, sending 64 at a time | 875,274 to 883,392 | 1,207,001 to 1,220,256 |

The client runs on the same machine and takes processors of its own, so
what a server on a machine of its own would give is not what this says.
Valkey was not measured in this run.
