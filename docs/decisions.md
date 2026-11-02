# Decisions

Technical choices that shape the code, and why they were made. A technology
without an entry here is not adopted.

## Dependencies

| Crate | Used for | Why this one |
|---|---|---|
| `crc32fast` | Checksum on every log record, so a torn or damaged record is detected on recovery | Widely used and maintained, MIT OR Apache-2.0, no dependencies beyond `cfg-if`. Hardware-accelerated where available, so the check costs little on the write path. A checksum is too small a thing to justify writing and verifying our own |
| `rustix` (dev, Unix only) | A plain `fsync` in the sync latency example, to compare it with `F_FULLFSYNC` | std has no way to call a plain `fsync` on macOS. Maintained by the Bytecode Alliance, Apache-2.0 / MIT, and safe to call, where `libc` would need `unsafe`. Not linked into the library |

## The log

- **A write is acknowledged only after its record is synced.** The log is the
  source of truth; the tree and the history are rebuilt from it.
- **Syncing uses `File::sync_data`**, which is `fdatasync` on Linux and
  `fcntl(F_FULLFSYNC)` on macOS. A plain `fsync` on macOS does not reach the
  disk.
- **A failed sync stops the log from accepting writes.** After a failed sync
  the state of unsynced bytes is unknown, and retrying could acknowledge data
  that was lost.
- **A damaged record at the end of the log is cut off; damage anywhere else
  stops the log from opening.** Appends are synchronous, so only the end can
  be torn by a crash. When a record does not check out, recovery looks for a
  whole, checked record anywhere after it: if one exists, acknowledged data
  was damaged and the log refuses to open rather than dropping it. Damage
  inside the very last record cannot be told apart from a torn write.
- **Opening the log always syncs before returning records.** A process that
  died between writing and syncing leaves bytes the kernel still serves but
  the disk may not hold.
- **The log file is locked by one writer at a time**, so two processes cannot
  interleave records.

### What a durable write costs

Measured with `cargo run --release --example sync_latency` (2,000 writes of
128 bytes per method, one sync per write). These are indicative numbers from
one machine, recorded to compare sync methods, not to state the speed of the
database.

Apple M4 MacBook Air (fanless, 32 GB), macOS 26.6.2, internal SSD with APFS,
on AC power, 2026-11-02:

| Method | p50 (µs) | p99 (µs) | max (µs) |
|---|---|---|---|
| `File::sync_data` (`F_FULLFSYNC`) | 4,015 | 5,387 | 10,667 |
| `File::sync_all` (`F_FULLFSYNC`) | 4,011 | 5,139 | 10,745 |
| plain `fsync(2)` | 25 | 47 | 79 |
| `Log::append` (framing, checksum, `F_FULLFSYNC`) | 4,011 | 5,275 | 10,841 |

A plain `fsync` is about 160 times faster on this machine because it stops at
the drive's cache; it is not durable across a power cut and is not used. The
log adds nothing measurable on top of the sync itself, so the cost of a
durable write on macOS is the cost of `F_FULLFSYNC`, about 4 ms. Reaching
memory speed for writes will need group commit: one sync for many writes.
- **All disk access goes through one storage boundary**, so deterministic
  simulation tests can replace the disk with one that loses unsynced bytes,
  tears writes, and fails syncs.
