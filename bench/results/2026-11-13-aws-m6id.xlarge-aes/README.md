# Results of 2026-11-13 on AWS, third run: the `aes` crate at 0.9.2

Not a release run. The profile of the same afternoon
([`../2026-11-13-aws-m6id.xlarge-next/`](../2026-11-13-aws-m6id.xlarge-next/))
had a sixth of a read's time in a function of the `aes` crate that sets
512-bit registers up for every call. The crate has done that on
processors with AVX-512 since 0.9.3; this run measures the server with
the crate held at 0.9.2 against the server as it was.

## Machine

EC2 `m6id.xlarge` in Tokyo, 4 virtual CPUs that are 2 cores with 2
threads each (Intel Xeon Platinum 8375C, which has AVX-512 and VAES),
Ubuntu 24.04.5, the logs on the local NVMe disk. Rust 1.98.0, Valkey
9.1.3's `valkey-benchmark` as the client. Synthetic data only, 32-byte
values, a million keys. `machine.txt` is from the machine, and holds the
one line that differs between the two sides.

## Files

| File | What | Code |
|---|---|---|
| `ab.csv` | `bench/ab.sh old new 3`: in turn, three rounds | `old`: `8a25150`, with `aes` 0.9.4. `new`: the same with `aes = "=0.9.2"` added to `crates/umbrachyx/Cargo.toml` and `cargo update -p aes --precise 0.9.2` |

## What it says

Medians of the three rounds, a second.

| | `aes` 0.9.4 | `aes` 0.9.2 | |
|---|---|---|---|
| `GET`s, 50 clients sending 16 at a time | 647,668 | 825,423 | +27% |
| Durable writes, 256 clients | 43,956 | 46,631 | +6% |
| Durable writes, 64 clients | 38,543 | 39,888 | +3% |
| `GET`s, one client | 53,908 | 55,648 | +3% |
| Durable writes, 50 clients sending 64 at a time | 69,672 | 71,426 | +3% |
| Durable writes, one client | 8,949 | 9,084 | +2% |
| Resident memory after 3.5 million writes | 2,224,908 KiB | 2,224,760 KiB | the same |

`GET`s from 50 clients gave 130,208, 166,667 and 152,439 on one side and
161,551, 113,895 and 114,090 on the other: that row depends on where the
benchmark's client lands, as the afternoon's run showed, and says
nothing about this change.

## How the run went

The instance ran from 06:47 to 07:09 UTC and was started from
CloudShell. The measurements began six minutes after it started: three
waiting for a person to confirm the host and hand the console over, and
two because the line that starts the script was typed but not run, which
was noticed on the first look at the log.
