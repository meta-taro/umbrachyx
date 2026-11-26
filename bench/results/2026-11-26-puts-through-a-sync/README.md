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
| `ci-runner-first.csv` | The same script on another runner, with the build one commit before the last of this change | |
| `ci-runner-waiting.csv` | An earlier run on such a runner, of the build before only (`d61a864` with switches for the trial): the two times a serving thread waits set to nothing and to more, one, two and four serving threads, and Valkey, three rounds; written by `ci-runner-waiting.sh` | |

The last column of `ci-runner.csv` and `ci-runner-first.csv`,
`held_by_serving_threads`, is 0 in every row: the builds it measured do not print that count. In
`ci-runner-waiting.csv` it is counted once a second, so it can pass the
row's requests by a second's worth.

## What it says

On the runner, lowest to highest of four rounds (`ci-runner.csv`):

| | Old | New | Valkey |
|---|---|---|---|
| `SET`, 64 clients: the slowest, ms | 94.0 to 96.7 | 3.0 to 4.3 in three rounds, 95.9 in one | 5.0 to 5.8 |
| `SET`, 256 clients: the slowest, ms | 95.4 to 98.4 | 4.2 to 5.3 in three rounds, 86.6 in one | 10.9 to 11.9 |
| `SET`, 8 clients: the slowest, ms | 72.7 to 94.8 | 2.4 to 3.1 in three rounds, 66.1 in one | 2.0 to 14.2 |
| `SET`, one client: the slowest, ms | 7.8 to 11.3 | 0.2 to 0.5 | 0.4 to 0.5 |
| `SET`s a second, one client | 46,189 to 51,760 | 48,170 to 49,702 | 32,916 to 34,507 |
| `SET`s a second, 8 clients | 87,108 to 98,749 | 91,158 to 102,041 | 94,937 to 96,587 |
| `SET`s a second, 64 clients | 87,673 to 96,749 | 93,809 to 97,031 | 91,258 to 97,144 |
| `SET`s a second, 256 clients | 89,880 to 94,286 | 91,878 to 98,736 | 84,517 to 97,276 |
| `SET`s a second, 50 clients sending 64 at a time | 772,798 to 807,102 | 983,284 to 1,034,661 | 970,874 to 1,011,122 |
| the same: 99th percentile, ms | 95.6 to 97.2 | 69.2 to 72.5 | 9.8 to 11.7 |
| `GET`s a second, 50 clients sending 16 at a time | 1,223,242 to 1,342,282 | 1,218,769 to 1,269,841 | 1,334,223 to 1,388,889 |

- **The stall is all but gone.** Before, once a second and whenever
  4 MiB was held, no write was answered for as long as the disk took:
  the slowest write of every row took about that long. Now the slowest
  is as short as Valkey's or shorter in three rounds of four; in one
  round of each of three rows a write still waited a sync's length.
  Which sync that is has not been found.
- **Writes sent 64 at a time are a quarter more**, and level with
  Valkey's.
- **From 8, 64 and 256 clients the new build's rounds overlap
  Valkey's.** Level, not ahead.
- **Still behind:** sent 64 at a time, one write in a hundred still
  waits about 70 ms, where Valkey's waits about 10. At a million writes
  a second on a disk this slow, more comes during one sync than may be
  held ahead of the disk, and the put that finds no room waits as
  before. And the `GET`s sent 16 at a time, which this change does not
  touch.

`ci-runner-first.csv` is the same on another runner a few hours
earlier, of the build before the last commit of this change: there the
puts beside the store had 4 MiB of their own, so twice as much could be
ahead of the disk. It gave the same picture (writes sent 64 at a time
798,085 to 840,336 before and 1,024,590 to 1,067,236 after, Valkey
1,063,264 to 1,083,424; the slowest write from 64 clients 74.8 to 96.4
ms before and 2.8 to 6.0 after).

On the laptop, lowest to highest of three rounds, writes a second:

| Clients | Old | New |
|---|---|---|
| 1 | 65,189 to 65,638 | 64,516 to 66,778 |
| 8 | 213,904 to 220,994 | 233,372 to 234,742 |
| 64 | 222,469 to 242,718 | 256,410 to 260,078 |
| 256 | 236,407 to 239,521 | 255,102 to 258,398 |
| 50, sending 64 at a time | 1,282,874 to 1,294,498 | 1,682,086 to 1,897,533 |

A third more sent 64 at a time, where 4 MiB filled several times a
second and each time a put waited for the disk; 6% to 9% more from 8,
64 and 256 clients; one client the same.

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
