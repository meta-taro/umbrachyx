# Results of 2026-11-26 on AWS: the writes to a plain-async table, before and after, and beside Valkey

Not a release run. It asks, on the machine the releases are measured
on, what the laptop and a small container had answered for the changes
made since `a9ed3fc` to the writes that are answered before they are on
disk (a `plain-async` table): whether they are faster there, and where
they stand beside Valkey with `appendfsync everysec`, the setting that
promises the same after a power cut.

## Machine

EC2 `m6id.xlarge` in Tokyo, 4 virtual CPUs (Intel Xeon Platinum 8375C),
16 GiB, Ubuntu 24.04.5 with Linux 7.0, transparent huge pages at
`madvise`, the logs on the local NVMe disk. Rust 1.98.0, Valkey 9.1.3
and its `valkey-benchmark` as the client. Synthetic data only, 32-byte
values, a million keys. `machine.txt` is from the machine.

For the rows of `two-machines.csv` the client was a second instance,
`m6id.2xlarge` (8 virtual CPUs), in the same availability zone, and the
servers listened on the first one's private address.

## Files

| File | What | Code |
|---|---|---|
| `ab.csv` | `bench/ab.sh old new 3`: the commit before the changes (`old`) and the one after (`new`) in turn, three rounds | `a9ed3fc` and `eea53bd` |
| `run-1/` to `run-3/` | `bench/umbrachyx.sh`, `bench/valkey.sh` and `bench/mixed.sh`, three times, client and server on the one machine | `eea53bd` |
| `serving-threads.txt` | The 64-client `SET` row again with one, two and four serving threads, with what each thread of the server used (`top`) and its context switches; written by `serving-threads.sh` | `eea53bd` |
| `two-machines.csv` | The same writes with the client on a machine of its own; `two-machines-servers.sh` started the servers, `two-machines-client.sh` drove them | `eea53bd` |
| `run.sh` | What was run on the machine, as it was run | |

Two things to know before reading the files:

- **`two-machines.csv` names two columns wrongly.** The column headed
  `p50_latency_ms` holds the 95th percentile and the one headed
  `p99_latency_ms` the maximum: the script cut the wrong fields out of
  what `valkey-benchmark` printed. The file is as the machine wrote it.
- The private addresses in the two `two-machines-*.sh` scripts were
  replaced with a name after the run.

## What it says: before and after

Medians of three rounds, a second (`ab.csv`).

| | `a9ed3fc` | `eea53bd` | |
|---|---|---|---|
| plain-async `SET`, one client | 29,560 | 56,964 | 1.9 times |
| plain-async `SET`, 8 clients | 108,873 | 110,132 | the same: one round of three gave 125,945 |
| plain-async `SET`, 64 clients | 106,440 | 118,133 | 11% more, every round |
| plain-async `SET`, 256 clients | 101,061 | 117,925 | 17% more, every round |
| plain-async `SET`, 50 clients sending 64 at a time | 557,569 | 987,654 | 1.8 times |
| plain `SET`, 50 clients sending 64 at a time | 546,747 | 605,877 | 11% more, every round |
| `GET`s, 50 clients sending 16 at a time | 888,099 | 941,176 | 6% more, every round |
| Durable writes with a history, 50 clients sending 64 at a time | 84,196 | 85,383 | the same |
| Durable writes with a history, 64 clients | 50,314 | 52,070 | 3% more, every round |
| `GET`s, 50 clients | 143,164 | 135,410 | neither: both sides ran from 116,000 to 164,000 |

The writes that keep a history were not what the changes were for, and
did not move.

## What it says: beside Valkey, on one machine

Medians of three runs, a second (`run-*/umbrachyx.csv`, `valkey.csv`,
`mixed.csv`). The client shares the four virtual CPUs with the server.

| | Umbrachyx, plain-async | Valkey, `everysec` | |
|---|---|---|---|
| `SET`, one client | 58,309 | 33,956 | 1.7 times |
| `SET`, 8 clients | 156,986 | 153,257 | the same: the runs overlap (but see below) |
| `SET`, 64 clients | 148,368 | 162,999 | **9% fewer**, every run |
| `SET`, 256 clients | 133,422 | 127,632 | the same: the runs overlap (but see below) |
| `SET`, 50 clients sending 64 at a time | 957,396 | 703,977 | 1.4 times |
| `GET`, one client | 60,569 | 37,864 | 1.6 times |
| `GET`, 50 clients sending 16 at a time | 1,302,083 | 1,213,592 | 7% more, every run |
| 95% reads, 50 clients | 185,207 | 108,151 | 1.7 times |
| 80% reads, 50 clients | 179,007 | 104,411 | 1.7 times |
| 50% reads, 50 clients | 172,265 | 102,668 | 1.7 times |

The three rows of mixed load use this repository's own client
(`mixed_load`), which has a thread for each connection; the others use
`valkey-benchmark` with its one thread.

## The row that loses, and why

From 64 clients sending one write at a time, on one machine, Valkey
answers 9% more. `serving-threads.txt` measured that row with the
number of serving threads set (two is what the server chooses on four
virtual CPUs), twice:

| | `SET`s a second, 64 clients | Processor the server used |
|---|---|---|
| Umbrachyx, one serving thread | 154,353 and 157,870 | one thread at 98% |
| Valkey | 147,080 and 148,331 | one thread at 97% |
| Umbrachyx, two serving threads | 132,855 to 145,780 (four runs) | two threads at 82% to 98% each |
| Umbrachyx, four serving threads | 115,053 and 116,158 | four threads at 37% to 52% each |

On this machine a second serving thread costs the row more than it
gives: two threads use nearly two processors and answer fewer writes
than one thread does with one. `valkey-benchmark` is one thread and
needs a processor of its own; so does the kernel's side of 64
connections. With the client on a machine of its own
(`two-machines.csv`, medians of three rounds) the order is the other
way round:

| | Umbrachyx, two serving threads | Valkey, `everysec` | |
|---|---|---|---|
| `SET`, one client | 17,470 | 15,642 | 12% more, every round |
| `SET`, 8 clients | 101,771 | 83,752 | 1.2 times |
| `SET`, 64 clients, the client with one thread | 151,172 | 150,421 | the same: the client's limit |
| `SET`, 64 clients, the client with four threads | 257,931 | 156,801 | 1.6 times |
| `SET`, 256 clients, the client with four threads | 249,813 | 159,859 | 1.6 times |
| `SET`, 50 clients sending 64 at a time, four threads | 912,617 | 694,203 | 1.3 times |
| `GET`, 50 clients sending 16 at a time, four threads | 1,332,445 | 1,142,041 | 1.2 times |

There, four serving threads gave 285,551 from 64 clients and one gave
173,807; one thread of Valkey gives what one thread gives. What the
two threads spend their time on when they answer less than one has not
been measured.

## The rows from several clients on one machine are not settled

The table beside Valkey above is what `bench/umbrachyx.sh` and
`bench/valkey.sh` gave. The same binary on the same machine gave less
in the two other files that measure those rows, each of which starts a
fresh server, fills it and goes straight to them:

| Plain-async `SET`s a second, `eea53bd` | `run-*/umbrachyx.csv` | `ab.csv`, `new` | `serving-threads.txt` |
|---|---|---|---|
| 8 clients | 152,439 to 161,290 | 109,469 to 125,945 | 117,018 (once) |
| 64 clients | 138,217 to 150,038 | 114,351 to 118,273 | 139,639 and 142,187 |
| 256 clients | 131,839 to 138,504 | 116,618 to 124,766 | 123,865 (once) |

`serving-threads.txt` measured Valkey the same way, and there the
default server lost all three: 117,018 against 140,924 from 8 clients,
139,639 and 142,187 against 147,080 and 148,331 from 64, and 123,865
against 145,356 from 256. A shared runner of this repository's CI, also
of four virtual CPUs, gave 58,928 against 84,317 from 8 clients for the
same commit with the release scripts.

Why `bench/umbrachyx.sh` gives more from the same server has not been
found. In it these rows come after a second of 50 clients writing;
whether that, or where the one thread of the client lands among four
virtual CPUs shared with two serving threads that stay awake, makes the
difference is not known. So on one machine: one client and the rows
sent many at a time are ahead in every file; **the rows from 8, 64 and
256 clients sending one write at a time are behind or level, depending
on the file**, and ahead only with the client on another machine.

One more thing these rows show and do not explain: the slowest single
write is slower for Umbrachyx in every row of `two-machines.csv`. From
64 clients it took 22 to 24 ms against 4 to 5 ms for Valkey, and from
256 clients 28 to 31 ms against 8 to 9 ms, while the 95th percentile
from 64 clients is lower for Umbrachyx (0.31 ms against 0.55 ms).

## How the run went

The instance ran from 02:12 to about 04:20 UTC: five minutes to
install and build, eleven for `bench/ab.sh`, 27 for the three runs,
then the rows above. It was started with a shutdown three hours ahead
and with `terminate` as what a shutdown does, and was terminated by
hand when the files had been copied off; so was the client. The files
were copied with `scp` and their sizes checked against what the
machine listed.
