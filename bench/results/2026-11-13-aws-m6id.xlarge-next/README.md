# Results of 2026-11-13 on AWS, second run: three questions

Not a release run. The run of the same morning
([`../2026-11-13-aws-m6id.xlarge-ab/`](../2026-11-13-aws-m6id.xlarge-ab/))
left three things open: whether a change measured only on a laptop holds
on the release machine, why `GET`s from 50 clients sometimes lose a
third, and where the writing thread's time goes on this processor.

## Machine

EC2 `m6id.xlarge` in Tokyo, 4 virtual CPUs that are 2 cores with 2
threads each (Intel Xeon Platinum 8375C; cpu0 shares a core with cpu2,
cpu1 with cpu3), Ubuntu 24.04.5, the logs on the local NVMe disk. Rust
1.98.0, Valkey 9.1.3's `valkey-benchmark` as the client, `perf` 7.0.14.
Synthetic data only, 32-byte values, a million keys. `machine.txt` is
from the machine.

## Files

`run.sh <scratch> f6cba74 78295e4` made all of them in one go, except
that its last step stopped: `perf` may not be used by an ordinary user
on this image. The profiles were taken again right after with
`kernel.perf_event_paranoid=-1`, by the same commands, and are the first
lines of what they printed.

| File | What |
|---|---|
| `ab.csv` | `bench/ab.sh old new 3`: `f6cba74` (`old`) and `78295e4` (`new`) in turn. Between them: a lookup in a node compares numbers before keys, and two changes that do nothing on this machine |
| `placement.txt` | `GET`s from 50 clients, six runs against each of two servers, with the server and the client held to chosen processors by `taskset` or left free |
| `pipelined-gets.txt` | `GET`s from 50 clients sending 16 at a time, three runs after each further million writes |
| `set-profile.txt` | Where the server's time goes while 50 clients send writes 64 at a time: by thread, and by function in the thread that writes |
| `get-profile.txt` | The same while 50 clients send `GET`s 16 at a time |

## What they say

**The lookup that compares numbers first holds here, a little**
(`ab.csv`, medians of three rounds, a second):

| | Before | After | |
|---|---|---|---|
| Durable writes, 64 clients | 35,518 | 37,872 | +7% |
| Durable writes, 256 clients | 40,225 | 42,248 | +5% |
| `GET`s, 50 clients sending 16 at a time | 605,510 | 629,921 | +4% |
| `GET`s, one client | 51,894 | 52,882 | +2% |
| Durable writes, 50 clients sending 64 at a time | 68,849 | 68,273 | the same |
| Durable writes, one client | 8,525 | 8,522 | the same |
| Resident memory after 3.5 million writes | 2,215,072 KiB | 2,224,676 KiB | +0.4% |

**`GET`s from 50 clients lose a third when the benchmark's client runs
on the core a serving thread is on** (`placement.txt`). With the server
held to one core and the client to the other, all twelve runs gave
160,000 to 164,000 a second. With both held to one core, all twelve gave
111,000 to 114,000. Left free, most runs gave about 160,000 and some
fewer; a client held to one processor beside a free server gave 111,000
to 134,000 for a whole server's six runs when the server's threads
happened to sit by it. So the unsteady row of the morning is where the
operating system puts three busy threads on two cores, not something the
server does differently from run to run. A client on another machine, or
a machine with more cores, would not see it; neither has been measured.

**`GET`s sent 16 at a time are not slower after more writes; fewer of
them miss** (`pipelined-gets.txt`). About 835,000 a second after a
million writes, 690,000 after two, 660,000 after three and 646,000 after
four. The writes pick keys at random among a million, so after the first
million about a third of the keys have never been written, and a `GET`
of a key that is not there has nothing to decrypt. The morning's rows
that read after 3.5 million writes, and the release table, measure reads
that find their key.

**The thread that writes has no one thing to fix** (`set-profile.txt`).
It has 93% of the server's processor time, and the most any function
takes of it is 9%: putting cells into the tree 8.8%, finding a key in a
node 6.8%, dropping the keys and nodes a write replaced 9.1% together,
looking up the replaced cell 4.0%, the allocator about 6%. Four
addresses `perf` could not name take 14% between them and also appear in
the read profile, where encryption is; hashing the commit is 3%.

**A read sets its cipher up again every time** (`get-profile.txt`).
While 50 clients send `GET`s 16 at a time the two serving threads have
all the server's time, and 77% of it is the server's own code. Finding
the key is 33% (18.7% in a node's index, 14.3% walking down). Expanding
the AES key, which depends only on the subject's key and not on the
value, is 12% in one function and about 16% with the two beside it; the
decryption itself is under 5%.

## How the run went

The instance ran from 05:59 to 06:28 UTC and was started from
CloudShell. The measurements began four minutes after it started, most
of that waiting for a person to confirm the host and hand the console
over. The script had been run end to end on another machine with small
counts first; it ran there as the administrator, which is why the limit
on `perf` was not met until here.
