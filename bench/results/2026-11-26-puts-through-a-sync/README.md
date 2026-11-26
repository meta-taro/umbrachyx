# Results of 2026-11-26: puts answered while the puts before them are synced

Not a release run. Two machines, the build before and the build after
in turn, to see what it changes that a `SET` to a plain-async table is
answered while the puts held earlier are being written, and that they
are written once they are half of what may be held ahead of the disk.

## Machines

- A laptop: Apple M4, 32 GiB, macOS 26.6.2, its own SSD, where a sync
  takes about a millisecond. Four serving threads (the default there).
- A shared runner of this repository's CI: four virtual CPUs (AMD EPYC
  9V45, two cores of two threads), Ubuntu, where a sync took most of a
  tenth of a second. Two serving threads (the default there). Shared
  machines differ from one run to the next by about three parts in ten;
  the builds were measured in turn within one run for that reason, and
  these numbers are not for comparing with any other run.

Rust 1.98.0, Valkey 9.1.3 and its `valkey-benchmark` as the client, on
the server's machine. Synthetic data only, 32-byte values, a million
keys.

## Files

| File | What | Code |
|---|---|---|
| `laptop-ab.csv` | `bench/ab.sh <old> <new> 3 plain-async` | old `d61a864`, new the commit that adds this folder |
| `ci-runner.csv` | `ci-runner.sh <old> <new> 4`: Valkey with `appendfsync everysec`, then old and new with the default serving threads and with one, four rounds. Each row: writes a second, the median, 99th percentile and slowest in milliseconds, what the client and the server used of the processors | the same two |
| `ci-runner.sh` | The script as it ran, from a branch made for the trial | |
| `ci-runner-waiting.csv` | An earlier run on such a runner, of the build before only (`d61a864` with switches for the trial): the two times a serving thread waits set to nothing and to more, one, two and four serving threads, and Valkey, three rounds; written by `ci-runner-waiting.sh` | |

The last column of `ci-runner.csv`, `held_by_serving_threads`, is 0 in
every row: the builds it measured do not print that count. In
`ci-runner-waiting.csv` it is counted once a second, so it can pass the
row's requests by a second's worth.

## What it says

On the runner, lowest to highest of four rounds:

| | Old | New | Valkey |
|---|---|---|---|
| `SET`, 64 clients: the slowest, ms | 74.8 to 96.4 | 2.8 to 6.0 | 5.3 to 8.8 |
| `SET`, 256 clients: the slowest, ms | 95.2 to 98.8 | 3.8 to 4.6 | 10.6 to 12.7 |
| `SET`, 8 clients: the slowest, ms | 69.2 to 96.3 | 1.0 to 3.8 | 4.8 to 23.8 |
| `SET`, one client: the slowest, ms | 9.0 to 12.4 | 0.2 to 1.8 | 0.2 to 1.0 |
| `SET`s a second, one client | 46,404 to 50,050 | 48,685 to 50,633 | 33,830 to 34,783 |
| `SET`s a second, 8 clients | 90,144 to 96,681 | 91,968 to 104,167 | 88,183 to 98,847 |
| `SET`s a second, 64 clients | 93,284 to 98,386 | 95,166 to 103,713 | 96,321 to 101,338 |
| `SET`s a second, 256 clients | 91,542 to 97,447 | 97,012 to 100,301 | 92,661 to 100,786 |
| `SET`s a second, 50 clients sending 64 at a time | 798,085 to 840,336 | 1,024,590 to 1,067,236 | 1,063,264 to 1,083,424 |
| the same: 99th percentile, ms | 97.0 to 98.0 | 68.4 to 77.8 | 8.5 to 11.9 |
| `GET`s a second, 50 clients sending 16 at a time | 1,232,286 to 1,382,170 | 1,247,661 to 1,428,572 | 1,403,509 to 1,432,665 |

- **The stall is gone.** Before, once a second and whenever 4 MiB was
  held, no write was answered for as long as the disk took: the slowest
  write of every row took about that long. Now the slowest is as short
  as Valkey's or shorter.
- **Writes sent 64 at a time are a quarter more**, and within a few
  parts in a hundred of Valkey's, still behind.
- **From 8, 64 and 256 clients the new build's rounds overlap
  Valkey's.** Level, not ahead.
- **Still behind:** sent 64 at a time, one write in a hundred still
  waits 68 to 78 ms, where Valkey's waits 9 to 12. At a million writes
  a second on a disk this slow, more comes during one sync than may be
  held beside the store, and the put that finds no room waits as
  before. And the `GET`s sent 16 at a time, which this change does not
  touch.

On the laptop, lowest to highest of three rounds, writes a second:

| Clients | Old | New |
|---|---|---|
| 1 | 63,492 to 65,041 | 66,291 to 66,556 |
| 8 | 218,341 to 220,507 | 230,947 to 234,467 |
| 64 | 240,385 to 244,200 | 233,645 to 256,739 |
| 256 | 236,967 to 243,605 | 252,525 to 256,739 |
| 50, sending 64 at a time | 1,274,697 to 1,289,491 | 1,515,152 to 1,763,668 |

More sent 64 at a time, where 4 MiB filled several times a second and
each time a put waited for the disk; a few parts in a hundred more
from 1, 8 and 256 clients; from 64 the rounds overlap.

## What was tried first and not kept

`ci-runner-waiting.csv` asked whether the serving threads' waiting was
what cost the rows from several clients. It was not. Medians of three
rounds, `SET`s a second from 64 clients: 85,281 as it is; 72,569 with a
thread sleeping at once where it stays awake 50 microseconds; 82,359
with a thread not looking for the turn to write; 79,962 staying awake
200 microseconds; 87,367 with one serving thread and 77,519 with four;
Valkey 85,704. In every one of those rows the client used 0.91 to 0.99
of a processor, and the slowest write took about 100 ms whatever was
switched: that led to the sync.

Neither machine is the one releases are measured on.
