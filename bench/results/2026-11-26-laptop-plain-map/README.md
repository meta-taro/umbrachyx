# Results of 2026-11-26 on a laptop: a key hashed once, a short value written over in place

Not a release run. One laptop, two builds in turn, to see what it
changes that the map of tables without history hashes a key once where
it hashed it twice, and overwrites a value of up to 1,024 bytes in its
own bytes where it replaced it with newly taken memory.

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2, its own SSD. Rust 1.98.0,
`valkey-benchmark` 9.1.3 as the client, on the same machine. Synthetic
data only, a million keys, a plain-async table.

## Files

| File | What | Code |
|---|---|---|
| `ab.csv` | `bench/ab.sh <old> <new> 3 plain-async`: 32-byte values, four serving threads (the default here) | old `5db70ee`, new the commit that adds this folder |
| `values.csv` | Other value lengths, and reads, by hand as below | the same two |

`values.csv`: each build in turn was started with `serve bench
--threads 2` on a fresh log with the table `kv` declared plain-async,
filled with `valkey-benchmark -n 1000000 -r 1000000 -c 50 -P 64 SET
kv/bench/key:__rand_int__ <value>`, and then given, with the same key
and a value of the length in the first column:

| Row | Arguments to `valkey-benchmark -r 1000000` |
|---|---|
| 8 clients, 64 clients | `-n 200000 -c 8` or `-c 64`, `SET` |
| 50 clients, 64 at a time | `-n 2000000 -c 50 -P 64`, `SET` |
| GET, 50 clients | `-n 200000 -c 50`, `GET` |
| GET, 50 clients, 16 at a time | `-n 2000000 -c 50 -P 16`, `GET` |

Five rounds for 128, 512 and 1,024 bytes, three for the reads and for
4,096 bytes.

## What it says

`ab.csv`, writes a second, lowest to highest of three rounds:

| Clients | Before | After |
|---|---|---|
| 1 | 63,674 to 65,210 | 63,633 to 65,062 |
| 8 | 214,362 to 223,214 | 217,628 to 222,717 |
| 64 | 228,833 to 243,013 | 228,050 to 245,399 |
| 256 | 239,234 to 239,808 | 236,967 to 239,521 |
| 50, sending 64 at a time | 1,188,354 to 1,194,743 | 1,262,626 to 1,301,236 |

`values.csv`, sent 64 at a time, thousands of writes a second:

| Value | Before | After |
|---|---|---|
| 128 bytes | 1,003 to 1,125 | 1,192 to 1,217 |
| 512 bytes | 415 to 479 | 435 to 466 |
| 1,024 bytes | 244 to 259 | 249 to 258 |
| 4,096 bytes | 86 to 88 | 86 to 88 |

So it is the short values sent many at a time that gain. From eight
clients that wait for each reply the ranges overlap at every length
(128 bytes: 221,000–229,000 before, 205,000–230,000 after; 512:
162,000–182,000 and 129,000–178,000; 1,024: 131,000–141,000 and
122,000–144,000; 4,096: 52,000–57,000 and 51,000–53,000), and so do
the reads (32 bytes, 16 at a time:
2,525,000–2,554,000 before, 2,574,000–2,601,000 after; 512:
1,764,000–1,778,000 and 1,604,000–1,778,000; 4,096: 500,000–530,000
and 494,000–504,000).

The client runs on the same machine. Valkey was not measured in this
run.
