# Results of 2026-11-13 on AWS: two changes tried on the release machine

Not a release run. Two changes were measured against the code before
them on the machine releases are measured on, because each had looked
different elsewhere: a new shape of leaf gave nothing on a CI runner and
cost a little on a laptop, and the number of serving threads only
mattered here.

## Machine

EC2 `m6id.xlarge` in Tokyo, 4 virtual CPUs that are 2 cores with 2
threads each (Intel Xeon Platinum 8375C), Ubuntu 24.04.5, the logs on the
local NVMe disk. Rust 1.98.0, Valkey 9.1.3 and its `valkey-benchmark` as
the client. Synthetic data only, 32-byte values, a million keys filled
before each set of rows. `machine.txt` is from the machine.

## Files

| File | What | Code |
|---|---|---|
| `leaf.csv` | `bench/ab.sh old new 3`: the code before (`old`) and the leaf that shares its keys (`new`), in turn, three rounds | `7343187` and `94801b0` |
| `threads.csv` | The code before with 4, 2 and 1 serving threads, each staying awake 0, 20, 50 or 200 µs before it sleeps, spinning (`n`) or giving the processor away between looks (`y`); two rounds. The wait was set by an environment variable in a build made only for this | `32c9ca1`, on the branch `feature/spin-before-sleep` |
| `candidate.csv` | `bench/ab.sh old new 3`: the code before and both changes together | `7343187` and `ad00761` (the same change as `f5eaf3c` on `develop`) |
| `candidate-umbrachyx.csv`, `candidate-valkey.csv`, `candidate-connections.csv` | `bench/umbrachyx.sh`, `bench/valkey.sh` and `bench/connections.sh` once each, with both changes | `ad00761` |
| `get-variation.txt` | `GET`s from 50 clients run again and again against one server, three servers of each of two builds (`cand`: both changes; `spin`: the old leaf, 2 threads, 50 µs), with the processor each thread was last on | `ad00761`, `32c9ca1` |

## What they say

Medians of the rounds, a second.

**The leaf that shares its keys** (`leaf.csv`):

| | Before | After | |
|---|---|---|---|
| Durable writes, 50 clients sending 64 at a time | 42,762 | 68,736 | +61% |
| Durable writes, 64 clients | 26,015 | 33,944 | +30% |
| Durable writes, 256 clients | 27,009 | 38,948 | +44% |
| Durable writes, one client | 7,468 | 7,949 | +6% |
| `GET`s, one client | 36,114 | 35,868 | the same |
| `GET`s, 50 clients | 100,503 | 99,751 | the same |
| `GET`s, 50 clients sending 16 at a time | 733,138 | 738,552 | the same |
| Resident memory after 3.5 million writes | 2,124,712 KiB | 2,215,504 KiB | +4% |

**Serving threads** (`threads.csv`, the two rounds agree to a few
percent):

| Threads, wait before sleeping | `GET`s, one client | `GET`s, 50 clients | `GET`s, 50 clients, 16 at a time |
|---|---|---|---|
| 4, none (as it was) | 37,011 | 101,409 | 872,023 |
| 4, 50 µs | 54,886 | 114,844 | 842,049 |
| 2, none | 37,137 | 145,535 | 762,863 |
| 2, 50 µs | 55,142 | 162,834 | 773,717 |
| 1, none | 37,151 | 160,716 | 404,672 |
| 1, 50 µs | 54,855 | 161,551 | 433,466 |

Waiting 20 µs gave the same as 50; 200 µs gave no more to reads and
took from writes (one client's durable writes fell from about 7,500 to
about 5,300 with 4 threads). Giving the processor away between looks
made no difference. Writes did not change with the threads or the wait.

**Both together, beside Valkey on the same day** (one run each):

| | Umbrachyx | Valkey (`appendfsync always`) |
|---|---|---|
| `GET`s, one client | 54,466 | 35,186 |
| Durable writes, one client | 8,813 | 6,737 |
| `GET`s, 50 clients | 156,740 | 161,290 |
| `GET`s while 50 clients write | 23,941 | 1,878 |
| Durable writes, 8 clients | 23,480 | 25,602 |
| Durable writes, 64 clients | 35,689 | 95,329 |
| Durable writes, 256 clients | 38,292 | 117,855 |
| `GET`s, 50 clients sending 16 at a time | 677,048 | 1,247,661 |
| Durable writes, 50 clients sending 64 at a time | 72,908 | 611,434 |
| `GET` p99 with 1,000 / 5,000 / 10,000 connections | 8.5 / 52 / 120 ms | 13.6 / 76 / 142 ms |

What got worse, or is not settled:

- **`GET`s from 50 clients are not steady with two serving threads.**
  Most runs give about 160,000 a second; about one in five gives 110,000
  to 145,000 (`get-variation.txt`, and two of the three rounds in
  `candidate.csv`). The build with the old leaf does the same, so it is
  not the leaf. On two cores with two threads each, where the two
  serving threads and the benchmark's one client land is the likely
  cause; that has not been shown.
- **The slowest percent with many connections is slower than with four
  threads**: 52 ms at 5,000 connections and 120 ms at 10,000, where the
  release run of the day before had 31 ms and 74 ms.
- **`GET`s sent 16 at a time are slower after many writes.** About
  625,000 a second in all three rounds of `candidate.csv`, which reads
  after 3.5 million writes, against about 735,000 before the changes;
  760,000 to 805,000 in `get-variation.txt`, which reads after about
  1.1 million. With two serving threads this row is held back by the
  server, so it shows what a read costs; why more writes make a read
  cost more has not been looked into.

## How the run went

The instance ran from 02:42 to 04:04 UTC and was started from CloudShell.
Nothing waited on a person for more than two minutes. The first script
stopped once before measuring, at a line that asked for the Rust version
outside a checkout, and was started again a minute later. A first run of
`bench/connections.sh` had another benchmark sent at its server by
mistake, on the same port; it was thrown away and run again alone, and
`candidate-connections.csv` is the second.
