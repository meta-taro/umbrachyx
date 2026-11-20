# Results of 2026-11-20 on a laptop: what opening a log takes

Not a release run. One laptop, to see what reading a log a record at a
time changes when a store opens: before, every record of the log was
decoded and held at once while it was replayed.

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2. Rust 1.98.0. Synthetic data
only, 32-byte values.

## Files

| File | What | Code |
|---|---|---|
| `open.csv` | `cargo run --release --example replica_memory`: a log of one pass and of two passes of 1,000,000 SETs over as many keys (written in groups of 64, 107 MB and 214 MB), opened to keep the history and to keep current values only, three times each | `before`: `9cbc27e`; `after`: the commit that adds this folder |

## What it says

The resident size right after opening, which is the most the process
held while it read the log:

| Log | Store keeps | Before | After | Opening took |
|---|---|---|---|---|
| one pass, 107 MB | the history | 1,313 MB | 945 to 947 MB | 6.6 to 6.8 s, then 6.9 to 7.1 s |
| one pass, 107 MB | current values only | 668 to 669 MB | 251 MB | 1.9 to 2.0 s both |
| two passes, 214 MB | the history | 2,511 to 2,544 MB | 1,810 to 1,819 MB | 11.0 to 12.0 s, then 11.3 to 11.5 s |
| two passes, 214 MB | current values only | 1,193 MB | 362 to 363 MB | 4.2 to 4.3 s both |

What is left over what the store holds afterwards is the file itself,
read whole before it is replayed. A first version of this change checked
every frame by decoding it and decoded it again to apply it; opening then
took 10 to 30% longer in runs made while the machine was busy, and that
version was not kept. Frames are now checked by length and checksum
first and decoded once.
