# Results of 2026-11-09: hashing a node only when a diff needs it

What it is worth to stop hashing nodes on every write, and what it costs
diffs. These numbers compare two versions of Umbrachyx on one machine; they
are not in the README's table, which is taken on fixed machines.

## Machines

- **Writes**: the Linux virtual machine of
  [`../2026-11-09-linux-vm-node-hash/`](../2026-11-09-linux-vm-node-hash/):
  Colima on an Apple M4 MacBook Air (32 GB), macOS 26.6.2, 2 virtual CPUs,
  4 GiB, Linux 6.8 (aarch64), a Debian 12 container. The client is
  `valkey-benchmark` from Valkey 9.1.3, on the same two CPUs as the server.
- **Diffs**: the MacBook Air itself, macOS 26.6.2.
- Rust 1.98.0 (the version the repository pins), `--release`.
- Synthetic data only.

## The binaries

| Name | Code |
|---|---|
| `base` | `ce37e20`: every node is hashed when a write makes it |
| `lazy` | the commit that adds this folder: a node is hashed the first time a diff asks for its hash, and the hash is kept |

## Files

| File | What | Command |
|---|---|---|
| `alloc.csv` | `base` then `lazy`, three rounds | `../2026-11-09-linux-vm-node-hash/alloc.sh <name> <binary>` |
| `alloc-overlapped.csv` | the same, an earlier try | as above, while the Mac was compiling at low priority. Rounds 2 and 3 are disturbed; kept, not used below |
| `old_reads-mac.txt` | `base` then `lazy`, three rounds | `cargo run --release --example old_reads` (200,000 commits) |

## What they show

Medians of three from `alloc.csv`, operations a second:

| Step | `base` | `lazy` |
|---|---|---|
| SET, 50 clients, pipelined 64 (first pass) | 67,981 | 167,785 |
| SET, 50 clients, pipelined 64 (second pass) | 61,690 | 133,511 |
| SET, 50 clients | 33,580 | 49,900 |
| GET, 50 clients | 186,220 | 191,205 |
| Bytes kept per SET | 3,092 | 3,093 |

- **Writes are 1.5 to 2.5 times faster.** Most nodes a write makes are
  replaced by a later write before anything compares them, and are now
  never hashed. The ranges of the three runs do not overlap for any SET
  row. GETs never hashed, and memory does not change.
- **Diffs of old commits are slower**, from `old_reads-mac.txt`: against
  the fifth commit before, a mean of 4.30 ms became 5.42 ms and the
  slowest of 300 went from about 10 ms to 15 ms. Against the parent, a
  diff reads the cells the commit wrote, not trees, and does not change.
- The first diff that reaches a tree pays for the nodes nobody has hashed
  yet, and two different roots are compared by hash, so that is the whole
  tree once. Later diffs reuse the kept hashes. In total no node is hashed
  more often than before.
