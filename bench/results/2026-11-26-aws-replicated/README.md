# Results of 2026-11-26 on AWS: a primary, its replicas and a client, each on a machine of its own

The first run of `bench/replicated.sh` on separate machines: what a
replica adds and what it costs, for Umbrachyx and for Valkey and
PostgreSQL set up the same way. Not a release run.

## Machines

Six EC2 instances in one availability zone of Tokyo, Ubuntu 24.04.5
with Linux 7.0, each with its logs on its local NVMe disk:

| Role | Type |
|---|---|
| Primary | `m6id.xlarge`, 4 virtual CPUs, 16 GiB |
| Four replicas | `m6id.xlarge` each |
| Client | `m6id.2xlarge`, 8 virtual CPUs, 32 GiB |

Rust 1.98.0, Valkey 9.1.3, PostgreSQL 16.15 (Ubuntu's). Commit
`eea53bd`. Synthetic data only, 32-byte values, a million keys, ten
seconds a row. The servers listened on private addresses only, and
nothing between them was encrypted.

## Files

| File | What |
|---|---|
| `replicas-1/`, `replicas-2/`, `replicas-4/` | What the client wrote with one, two and four of the replicas: `load.csv`, `lag.csv`, `run.txt` |
| `setup.sh` | What every machine ran: installs, builds, then its role of `bench/replicated.sh` |
| `client.sh` | What the client ran: the three passes in turn |

Each kind of row was measured once in each pass. The private addresses
in `run.txt` and `client.sh` were replaced with names after the run.
A person copied the primary's `seed` directory to each replica, since
it holds encryption keys.

## What it says

A row is to be read beside the row that promises the same when a write
is answered: `u-history` beside `valkey` (`appendfsync always`) and
`postgres` — the primary's disk — and `u-waits` beside `valkey, WAITAOF
1 1` and `postgres, a synchronous replica` — the primary's disk and a
replica's.

Reads a second from the replicas, 50 readers on each:

| Replicas | Umbrachyx | Valkey | PostgreSQL |
|---|---|---|---|
| 1 | 247,961 | 192,576 | 86,843 |
| 2 | 452,243 | 367,354 | 176,942 |
| 4 | 657,152 | 641,189 | 353,638 |

With four replicas the client's eight virtual CPUs were probably what
was measured for Umbrachyx and Valkey: neither doubled from two. The
client's processor was not recorded.

Reads a second from the replicas while eight clients write to the
primary:

| Replicas | Umbrachyx | Valkey | PostgreSQL |
|---|---|---|---|
| 1 | 217,431 | 88,425 | 79,964 |
| 2 | 417,112 | 180,869 | 161,078 |
| 4 | 634,102 | 356,469 | 324,193 |

Valkey's replicas read about half as much while they apply writes;
Umbrachyx's lose 4% to 12%. Its primary took 15,480, 16,905 and 12,919
writes a second meanwhile, Valkey's 22,200, 21,180 and 18,935.

Writes a second to the primary, answered once on its disk (the three
passes, with one, two and four replicas following):

| Writers | Umbrachyx, `u-history` | Valkey | PostgreSQL |
|---|---|---|---|
| 8 | 20,016 / 20,215 / 20,110 | 23,271 / 22,847 / 18,275 | 19,233 / 19,426 / 19,489 |
| 64 | 33,865 / 38,611 / 35,756 | 84,575 / 84,638 / 72,917 | 42,536 / 42,467 / 42,982 |
| 256 | 42,115 / 53,385 / 38,849 | 136,937 / 136,334 / 129,129 | 37,832 / 38,202 / 32,299 |

Writes a second answered once on the primary's disk and a replica's:

| Writers | Umbrachyx, `u-waits` | Valkey, `WAITAOF 1 1` | PostgreSQL, a synchronous replica |
|---|---|---|---|
| 8 | 10,727 / 10,262 / 10,614 | 11,384 / 11,272 / 11,255 | 12,359 / 12,257 / 12,255 |
| 64 | 26,324 / 30,402 / 27,824 | 43,421 / 43,705 / 42,179 | 35,090 / 35,011 / 35,315 |
| 256 | 34,903 / 36,896 / 37,424 | 72,060 / 70,941 / 69,114 | 34,630 / 33,042 / 28,943 |

**Umbrachyx loses the writes from many clients**: from 256, it answers
three to four tenths of what Valkey does on the primary's disk, and
half with a replica's disk waited for. From eight clients the three are close.
These are the writes that keep a history; they are the same rows that
lose on one machine
([`../2026-11-26-aws-m6id.xlarge-speed/`](../2026-11-26-aws-m6id.xlarge-speed/)).

How long after a write is answered a replica has it, in milliseconds,
median and 99th percentile of a thousand writes (`lag.csv`):

| Replicas | `u-history` | `u-waits` | `valkey` | `valkey, WAITAOF 1 1` |
|---|---|---|---|---|
| 1 | 0.154, 0.335 | 0.141, 0.229 | 0.160, 0.299 | 0.138, 0.202 |
| 2 | 0.149, 0.315 | 0.143, 0.238 | 0.160, 0.247 | 0.137, 0.207 |
| 4 | 0.135, 0.284 | 0.145, 0.223 | 0.156, 0.232 | 0.140, 0.264 |

The same, to within what one pass can tell. The slowest write of a
thousand took 110 ms for `u-history` and 179 ms for `u-current` in the
first pass, and under 1 ms in the third; Valkey's slowest was 1.9 ms.
PostgreSQL's lag is what its primary reports (`replay_lag`), another
way of measuring, and is in the files.

## How the run went

The six instances ran from 03:00 to about 04:25 UTC, each started with
a shutdown three hours ahead and `terminate` as what a shutdown does,
and were terminated by hand when the files had been copied off. About
seven minutes went to installing and building, eleven to waiting for a
person to hand each machine's console over in turn and to copy the
seed, and seven to each of the client's first two passes.
