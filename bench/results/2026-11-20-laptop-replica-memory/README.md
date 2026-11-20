# Results of 2026-11-20 on a laptop: what a replica holds in memory

Not a release run, and not the fixed measuring machines: one laptop, to
decide whether a replica that keeps current values only
(`serve --follow --current-values-only`) is worth having. Two servers on
one machine say nothing of two machines.

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2, both servers and the client on
it, their logs on its one disk. Rust 1.98.0, Valkey 9.1.3's
`valkey-benchmark` as the client. Synthetic data only, 32-byte values.

## Files

| File | What | Code |
|---|---|---|
| `replica_memory.csv` | `bench/replica_memory.sh 200000` three times, then `bench/replica_memory.sh 1000000` once | the commit that adds this folder |

## What it says

A primary is sent `sets` SETs over a space of as many keys, then as many
again; its replica follows on the same machine, and its resident size is
read once it holds every record.

| Replica | SETs | Held after the second pass | Per SET of the second pass | Answers again after a restart in |
|---|---|---|---|---|
| keeps the history | 2 × 200,000 | 300 MB (three runs: 300,416 to 300,880 KiB) | 666 to 668 bytes | 1.23 to 1.25 s |
| keeps current values only | 2 × 200,000 | 55 MB (54,688 to 56,320 KiB) | 36 to 47 bytes | 0.61 to 0.68 s |
| keeps the history | 2 × 1,000,000 | 1,432 MB | 676 bytes | 9.3 s |
| keeps current values only | 2 × 1,000,000 | 193 MB | 40 bytes | 3.2 s |

The last column is the time to read the whole log again, which both do:
only what is built from it differs. The resident size after a restart
(`rss_kib_restarted`) is higher than before it for both, since reading
the log takes memory that the process keeps for a while.

Reads are not faster: in the process, with a million keys, a read of a
current value costs 460 to 510 ns from either
(`cargo run --release --example replica_memory`, same laptop).
