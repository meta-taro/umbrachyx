# Results of 2026-11-17 on AWS: two commits in turn, to tell the machine from the code

Not a release run. The release run of the same day
([`../2026-11-17-aws-m6id.xlarge/`](../2026-11-17-aws-m6id.xlarge/)) gave
less than the run of the day before on the rows both have, and
Umbrachyx's rows fell further than Valkey's. This measures the commit of
the day before and the commit of this day in turn on one more instance
of the same type, to see whether the code or the instance was slower.

## Machine

EC2 `m6id.xlarge` in Tokyo, 4 virtual CPUs (Intel Xeon Platinum 8375C),
16 GiB, Ubuntu 24.04.5 with Linux 7.0, transparent huge pages at
`madvise`, the logs on the local NVMe disk. Rust 1.98.0, Valkey 9.1.3's
`valkey-benchmark` as the client. Synthetic data only, 32-byte values, a
million keys filled before each set of rows. `machine.txt` is from the
machine: the two commits, the kernel, the processor and the memory.

## Files

| File | What | Code |
|---|---|---|
| `ab.csv` | `bench/ab.sh old new 3`: the commit measured the day before (`old`) and the commit measured this day (`new`), in turn, three rounds. Every row keeps its history; `old` has no other kind of table | `60c6584` and `20b1673` |

## What it says

Medians of the three rounds, a second.

| | `60c6584` | `20b1673` | |
|---|---|---|---|
| Durable writes, 50 clients sending 64 at a time | 82,082 | 79,510 | 3% fewer |
| Durable writes, 64 clients | 48,321 | 47,562 | 2% fewer |
| Durable writes, 256 clients | 54,274 | 54,511 | the same |
| Durable writes, one client | 9,298 | 9,246 | the same |
| `GET`s, one client | 54,526 | 54,201 | the same |
| `GET`s, 50 clients | 156,740 | 156,740 | the same |
| `GET`s, 50 clients sending 16 at a time | 819,336 | 825,764 | the same |
| Resident memory after 3.5 million writes | 2,246,004 KiB | 2,244,724 KiB | the same |

- **The two commits are within 3% of each other**, and both give what
  the release run of this day gave (84,221 writes sent 64 at a time,
  860,585 `GET`s sent 16 at a time), not what the run of the day before
  gave from the older commit (109,951 and 1,166,181). The instance of
  the day before was the faster one.
- **Writes sent 64 at a time are 3% fewer with the newer commit**, in
  each of the three rounds (79,148, 79,510 and 79,859 against 81,041,
  82,082 and 82,512); from 64 clients every round of the newer is below
  every round of the older, 2% at the medians. Which of the changes
  between the two commits costs it was not measured; they are the ones
  that added tables without a history.
- One round of the newer commit gave 109,709 `GET`s from 50 clients
  where the other five gave 152,555 to 157,604: the row that moves when
  the benchmark's client lands on a serving thread's core
  ([`../2026-11-13-aws-m6id.xlarge-next/`](../2026-11-13-aws-m6id.xlarge-next/)).

## How the run went

The instance ran from 03:58 to 04:13 UTC: about 9 minutes to install
and to build Valkey and both commits, about 5 for the three rounds.
Both files were read from the machine and their sizes checked against
its listing.
