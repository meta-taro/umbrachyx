# A spike on 2026-11-16: a table without history, before it is built

Not a release measurement. `bench/spike-plain.sh <nvme dir> 2` on the
release machine (EC2 `m6id.xlarge`, Tokyo, the logs on its local NVMe),
at the spike commit 6137277: the cheapest write a table without history
could do, with the sync waited for and with the write answered first,
beside Valkey 9.1.3 with `appendfsync always`, `everysec` and no
append-only file. Two rounds; `spike.csv` has both. Means of the two:

| Writes a second | History | Plain, synced | Plain, answered first (sync every 10 ms) | Valkey `always` | Valkey `everysec` | Valkey, no file |
|---|---|---|---|---|---|---|
| 1 client | 8,859 | 10,497 | 27,871 | 6,418 | 34,217 | 37,565 |
| 8 clients | 27,224 | 40,470 | 103,179 | 24,565 | 137,149 | 166,615 |
| 64 clients | 44,353 | 76,879 | 105,486 | 95,420 | 159,116 | 160,385 |
| 256 clients | 53,448 | 86,525 | 105,573 | 138,651 | 132,170 | 152,499 |
| 50 clients, 64 at a time | 85,769 | 115,363 | 112,279 | 584,486 | 696,673 | 1,153,315 |
| The same with fewer keys, no token | 132,267 | 172,209 | 184,715 | | | |
| The same with a capability token | 42,252 | 50,562 | 64,606 | | | |

Reads did not change with the table's kind. Syncing every 1,000 ms
instead of every 10 gave the same rows.

What it says: taking the history away lifts writes by a third to three
quarters, and answering before the sync brings separate clients to
within 1.2 to 1.5 times of Valkey's `everysec`. But every variant stops
near 105,000 to 115,000 writes a second with a million keys, whatever
the clients and whether or not the sync is waited for, so what bounds
it is neither the history nor the disk; and writes sent 64 at a time
stay 5 to 6 times behind Valkey. A capability token bounds an agent's
writes well below that.

The instance ran from 05:17 to 05:54 UTC; the measurement started six
minutes after it was up.

## Where the time goes (`profile.txt`)

`bench/spike-profile.sh <nvme dir>` on another instance of the machine,
at dfbd56c: each variant under load, sampled with `perf` for 12 s. The
instance ran from 06:00 to 06:20 UTC; the measurement started nine
minutes after it was up.

- **Writes sent 64 at a time (113,000 a second, plain, either way of
  syncing)**: the one thread that writes is at 97 to 98% of a processor
  and everything else is idle (the serving threads 6% each, the
  benchmark's client 4%). Three quarters of that thread is the map: 28
  to 30% freeing the nodes each write replaces, 19% making the new
  path, 16% copying a node's entries, about 8% in the allocator. The
  map is the persistent one a latest table uses, which copies the path
  to a key on every write so that readers keep the old one. Sealing the
  value, the checksum and building the record are under 5% together.
- **64 clients one at a time (101,000 a second)**: both serving threads
  and the benchmark's client are at 93 to 94% each, the writing thread
  at 90%. Two fifths of a serving thread is system calls: waiting for
  events (a quarter to a third) and one `send` per reply (a sixth).
- **A table with history, 64 at a time (87,000)**: the writing thread
  again, spread over the tree: searching and inserting about a third,
  reading the replaced cell 8%, freeing nodes 6%, hashing the commit 4%.
- **With a capability token (63,000)**: the two serving threads are at
  98 to 99% and the writing thread at 46%. 77% of a serving thread is
  checking the token, and nearly half of that is building the
  authorizer again for each write.

## Overwritten in place (`spike-in-place.csv`)

The same script at 2684193, where the spike's keys live in a sharded
hash map that a write changes in place and a read locks for one lookup,
on a third instance of the machine (07:16 to 07:41 UTC of an instance
up from 06:53; a person's confirmation took 23 minutes). Means of two
rounds:

| A second | Plain, synced | Plain, answered first (sync every 10 ms) | Valkey `always` | Valkey `everysec` | Valkey, no file |
|---|---|---|---|---|---|
| Writes, 1 client | 10,813 | 29,645 | 6,183 | 33,531 | 36,246 |
| Writes, 8 clients | 46,439 | 103,987 | 23,886 | 113,840 | 166,599 |
| Writes, 64 clients | 91,854 | 104,298 | 94,143 | 163,935 | 158,495 |
| Writes, 256 clients | 110,589 | 104,441 | 124,150 | 157,361 | 154,099 |
| Writes, 50 clients, 64 at a time | 499,506 | 536,772 | 606,793 | 664,719 | 1,112,496 |
| `GET`s, 50 clients, 16 at a time | 1,245,849 | 1,227,060 | 999,753 | 1,120,828 | 1,161,497 |
| `GET`s, 1 client | 60,122 | 61,520 | 38,053 | 37,867 | 39,098 |
| `GET`s while 50 clients write | 20,434 | 32,001 | 1,753 | 9,034 | 24,271 |
| Writes with fewer keys, no token / with a token | 389,371 / 53,060 | 398,830 / 66,918 | | | |

Writes sent 64 at a time went from about 113,000 a second to 500,000
with the sync waited for and 537,000 without, against Valkey's 607,000
and 665,000: 1.2 times behind where it was 5 to 6. `GET`s sent 16 at a
time are ahead of Valkey's on every setting. With the sync waited for,
separate clients are ahead at 1 and 8 clients, level at 64 and 1.1
times behind at 256. Answered first, separate clients still stop near
104,000 a second, 1.5 times behind `everysec` at 64 clients: the
serving threads and their system calls, as the profile above said. A
token still bounds an agent's writes near 53,000 to 67,000.
