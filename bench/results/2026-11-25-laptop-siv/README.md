# Results of 2026-11-25 on a laptop: what sealing and opening one value take

Not a release run. One laptop, to see what setting a key up once
changes: before, the store sealed and opened through RustCrypto's
`aes-siv`, which expands the key of the counter mode for every value;
now it puts the same construction together itself (`store/siv.rs`).

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2. Rust 1.98.0. Synthetic values,
one thread, nothing else measured at the time.

## Files

| File | What | Code |
|---|---|---|
| `seal-cost.txt` | `cargo test --release -p umbrachyx --lib what_sealing_costs -- --ignored --nocapture`, three times | the commit that adds this folder |

Each line is one value length and one operation: nanoseconds for one
value with `aes-siv` 0.8.0, and with the store's own. Both are timed in
the same process, one after the other, over the same value and key.

## What it says

Lowest to highest of the three runs, nanoseconds for one value:

| Value | Operation | `aes-siv` | The store's own |
|---|---|---|---|
| 32 bytes | seal | 185 to 191 | 56 to 57 |
| 32 bytes | open | 181 to 196 | 72 to 73 |
| 256 bytes | seal | 267 to 274 | 183 to 210 |
| 256 bytes | open | 285 to 316 | 183 to 186 |
| 4,096 bytes | seal | 2,426 to 2,473 | 2,297 to 2,385 |
| 4,096 bytes | open | 2,461 to 2,503 | 2,307 to 2,359 |

What is saved is the same for every length, about 85 to 150 ns a
value, so it is most of the cost of a short value and a twentieth of a
long one's.

This times the cipher alone. What it does to a server's reads and
writes is not in this folder.
