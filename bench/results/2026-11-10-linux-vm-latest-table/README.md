# Results of 2026-11-10: a table that keeps only its latest values

The same virtual machine and the same memory script as
[`../2026-11-09-linux-vm-sparse-trees/`](../2026-11-09-linux-vm-sparse-trees/),
now with a third row: the same SETs to keys in a table declared to keep
only the latest value of each key (`umbrachyx table pos latest`). They
are behind the memory table in
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

Each pass is 200,000 SETs of 32-byte values over as many keys, sent twice.
Sizes are the resident size of the server process in KiB, as `ps`
reports it; the last column is how much the second pass added per SET, in
bytes.

## What the rows are

- `umbrachyx`: keys `key:<n>`, in no table, so they keep their history,
  as in the folders before.
- `umbrachyx (latest table)`: keys `pos/bench/key:<n>`, in the table
  `pos`, declared latest before the first write. The keys share one
  subject (`pos/bench`), as `key:<n>` do, so each run makes one
  encryption key; with one subject per key, making and syncing a key for
  every new key would be what is measured.
- `valkey (appendfsync always)`: keys `key:<n>`.

The server writes to a branch (`bench`), as every server does. Off
`main`, a latest table still keeps each commit's written values and the
ones they replaced, so a merge can find conflicts; part of the latest row
is those. Every commit also keeps its own record (who, why, when, its
parent), whatever the engine.
