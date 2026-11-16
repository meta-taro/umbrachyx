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
