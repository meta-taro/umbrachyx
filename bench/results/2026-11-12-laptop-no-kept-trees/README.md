# Results of 2026-11-12: no commit keeps a whole tree

Before this, one commit in 1,024 along each line of history kept its
whole tree, and the tree of any other was rebuilt from the nearest kept
one. Now only the head of each branch has a tree. A past state is made
when asked for, from the head of its branch or from its start, whichever
is nearer; a diff between two commits on one line is read from the cells
written between them; a key's value at an old commit is found along the
key's own chain of cells. `docs/decisions.md`, "The store", has the
reasons.

## Machine

- An Apple M4 MacBook Air (32 GB), macOS 26.6.2, for every file but one.
- `memory-linux-vm.csv`: a Linux virtual machine on that laptop (Colima
  with the macOS Virtualization framework, 2 virtual CPUs, 4 GiB of
  memory, Linux 6.8, aarch64), in a Debian 12 container, with Valkey
  9.1.3 built from its release tag.
- Rust 1.98.0, built with `--release`. Synthetic data only. One run of
  each, three of `commit_cost`.

## Files

| File | Command | Code |
|---|---|---|
| `memory-cost.txt` | `cargo run --release -p umbrachyx --example memory_cost` | the commit that adds this folder |
| `old-reads.md` | `cargo run --release -p umbrachyx --example old_reads` | the same |
| `commit-cost.txt` | `cargo run --release -p umbrachyx --example commit_cost` | the same |
| `memory-linux-vm.csv` | `bench/memory.sh` | the same |
| `*-before.*` | the same examples | `e74cbde`, the commit before, with this commit's `old_reads` example |

## What they say

| | Before | After |
|---|---|---|
| Memory a one-key commit leaves held, counted by the allocator | 3,405 bytes | 619 bytes |
| Resident memory per SET over the Redis protocol, second pass (Linux VM) | about 3,200 bytes (`../2026-11-09-linux-vm-sparse-trees/`) | 620 bytes |
| CPU of a one-key commit with the disk taken out | 3.81 to 3.88 µs | 3.69 to 3.74 µs |
| Diff of an old commit against its parent, mean / slowest | 0.007 / 0.022 ms | 0.004 / 0.021 ms |
| against the fifth commit before it | 5.1 / 13.8 ms | 0.007 / 0.014 ms |
| against the thousandth commit before it | 4.9 / 9.9 ms | 0.44 / 1.2 ms |
| One key's value at an old commit | 0.048 / 0.139 ms | 0.002 / 0.009 ms |
| **Everything an old commit held, against nothing** | **3.7 / 8.2 ms** | **43.5 / 59.8 ms** |

The last row is what got slower: the whole state of a commit far from
both ends of its history, here 200,000 commits long. It costs the
commits between it and the nearer end, so it grows with the history.

Valkey holds 17 bytes for the same second pass, since it keeps only the
latest value of each key. A table declared `latest` on a branch that is
not `main` now holds as much per SET as one that keeps its history
(618 against 620 bytes): off `main` it keeps the cells a branch wrote,
for merges. On `main` it keeps none.
