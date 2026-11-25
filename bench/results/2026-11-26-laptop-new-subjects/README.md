# Results of 2026-11-26 on a laptop: writes to new subjects on a plain-async table

Not a release run. One laptop, two builds in turn, to see what it
changes that a new subject's key is held with the put that needs it and
written before it, where every new subject synced the key file at once.

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2, its own SSD. Rust 1.98.0,
`valkey-benchmark` 9.1.3 as the client, on the same machine. Four
serving threads (the default here). Synthetic data only, 32-byte values.

A sync on macOS is a full flush of the disk and takes about a
millisecond, so what a sync for every new subject costs is larger here
than on Linux.

## Files

| File | What | Code |
|---|---|---|
| `ab.csv` | `PLAIN_KEY=kv/__rand_int__/v bench/ab.sh <old> <new> 3 plain-async` | old `87fae0e`, new the commit that adds this folder |

The key is `kv/<a number below a million>/v`: every key a subject of its
own. Each round starts on a fresh log, so nearly every write of a round
is to a subject that has no key yet.

## What it says

Writes a second, lowest to highest of the three rounds:

| Clients | Before | After |
|---|---|---|
| 1 | 690 to 691 | 39,277 to 40,040 |
| 8 | 1,270 to 1,271 | 124,766 to 129,618 |
| 64 | 7,321 to 7,362 | 195,503 to 198,610 |
| 256 | 27,766 to 28,054 | 197,044 to 203,666 |
| 50, sending 64 at a time | 205,065 to 220,580 | 521,105 to 523,697 |

Before, the rate was the rate of syncs of the key file: one for each
group of writes that brought a new subject. After, the key file is
written when the puts held are, once a second unless something else is
written first.

With one subject for every key the same table gives 63,000 to 65,000
from one client and 1,260,000 to 1,300,000 sent 64 at a time on this
laptop (`../2026-11-26-laptop-plain-map/`): a new subject still costs a
write, the key made for it and the first value sealed by the store
rather than by the thread that read it, and no longer a sync.
