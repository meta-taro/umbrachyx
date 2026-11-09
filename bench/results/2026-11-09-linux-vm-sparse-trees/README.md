# Results of 2026-11-09: commits that keep only the cells they wrote

The same virtual machine and the same scripts as
[`../2026-11-09-linux-vm/`](../2026-11-09-linux-vm/), after the store
stopped keeping a whole tree for every commit: most commits now keep the
cells they wrote, and one in 1,024 along each line of history keeps its
tree. They are behind the memory table in
[`docs/comparison.md`](../../../docs/comparison.md).

## Machine

- A Linux virtual machine on an Apple M4 MacBook Air (32 GB), macOS
  26.6.2: Colima with the macOS Virtualization framework, 2 virtual CPUs,
  4 GiB of memory, Linux 6.8 (aarch64). Everything ran in a Debian 12
  container inside it.
- Rust 1.98.0 (the version the repository pins), built with `--release`.
- Valkey 9.1.3 built from its release tag, default build (jemalloc). The
  client for both servers is that release's `valkey-benchmark`.
- Synthetic data only.

## Files

| File | Command | Code |
|---|---|---|
| `run-1/`, `run-2/`, `run-3/` `memory.csv` | `bench/memory.sh` | the commit that adds this folder |
| `old-reads.md` | `cargo run --release -p umbrachyx --example old_reads` | the commit that adds this folder |
| `old-reads-before.md` | the same example | `9e08bb9`, the commit before, with the example copied in |

The memory runs are 200,000 SETs a pass, as in the folder before. The
reads are diffs between commits of a history of 200,000 one-key commits;
the example's comment says how it writes them. Times are in milliseconds.
