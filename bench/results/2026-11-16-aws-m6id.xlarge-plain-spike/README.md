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
