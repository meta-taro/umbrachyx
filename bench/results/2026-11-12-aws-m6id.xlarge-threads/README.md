# Results of 2026-11-12 on AWS: where the time goes with many writers

A second, short run on the release machine the same day, to find why the
rows with many writers were so far behind there
([`../2026-11-12-aws-m6id.xlarge/`](../2026-11-12-aws-m6id.xlarge/)) when
a CI runner with as many virtual CPUs gave more than twice as much.

## Machine

The same kind as the run before: EC2 `m6id.xlarge` in Tokyo, 4 virtual
CPUs that are 2 cores with 2 threads each (Intel Xeon Platinum 8375C),
Ubuntu 24.04.5, the logs on the local NVMe disk. Rust 1.98.0, Valkey
9.1.3's `valkey-benchmark` as the client, `perf` from Ubuntu's
`linux-tools`. Synthetic data only. One run of each.

## Files

| File | Command | Code |
|---|---|---|
| `threads.txt` | `bench/threads.sh` | fc62b6f |
| `get-profile.txt` | `perf stat` and `perf record` on the server while 50 clients send `GET`s, with 1 and with 4 serving threads | fc62b6f |

## What they say

- **Writes are held back by the thread that writes, and half of its time
  is copying leaves and freeing them.** With 50 clients sending writes 64
  at a time, that thread has 96% of the server's processor time. In it,
  29% is dropping the tree nodes a write replaced and 23% is cloning a
  leaf's entries: a leaf is 32 keys and 32 values, each counted up when
  the leaf is copied for one changed value and counted down when the old
  leaf goes. Inserting is another 7%, looking keys up 3%, encrypting
  about 4%, hashing about 2%. On the laptop and in its virtual machine
  the same counting is cheap, which is why it did not stand out there.
- **The number of serving threads does not change writes**: 28,000 to
  30,000 a second from 64 clients and about 42,000 sent 64 at a time,
  with 1, 2 or 4.
- **It changes reads, both ways.** `GET`s from 50 clients: 157,109 a
  second with 1 serving thread, 126,422 with 2, 100,755 with 4, the
  default here; Valkey gave 156,495 in the run before. Sent 16 at a
  time: 399,760, 729,394 and 811,359.
- **With 4 threads the server spends its time going to sleep and waking
  up**: 260,374 context switches and 21,563 moves between processors in
  5 seconds, against 83 and none with 1 thread, for about the same
  processor time. Each thread has few connections ready at a time,
  answers them and waits again.

## How the run went

The instance ran from 05:37 to 06:08 UTC. The console's launch page
would not scroll, so the instance was started from CloudShell with the
same settings. The local NVMe disk was `nvme0n1` this time and the root
disk `nvme1n1`, the other way round from the run before; `mkfs` refused
the root disk, and the disk was then picked by its size.
