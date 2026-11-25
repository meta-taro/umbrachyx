# Results of 2026-11-25 on a laptop: what checking and restoring a backup take

Not a release run. One laptop, to see what reading a backup's copy of
the log a part at a time changes: before, `restore` and `backup
<directory> --check` read the whole copy into memory and decoded every
record of it at once.

## Machine

A laptop: Apple M4, 32 GiB, macOS 26.6.2, its own SSD. Rust 1.98.0,
Valkey 9.1.3 for the client that writes the SETs. Synthetic data only,
32-byte values.

## Files

| File | What | Code |
|---|---|---|
| `before.txt` | `UMBRACHYX_BIN=<a binary built from 85fbd78> bench/restore.sh 1000000` | `85fbd78` |
| `after.txt` | `bench/restore.sh 1000000` | the commit that adds this folder |

Both runs back up a store of 1,000,000 SETs over as many keys, written
by the server built from the commit that adds this folder; the copy of
the log is 131,008,871 bytes. Each command is run three times.

## What it says

The most memory the process held, and how long it took:

| Command | Before | After |
|---|---|---|
| `backup <directory> --check` | 590 MB, 0.25 to 0.26 s | 8 MB, 0.26 to 0.27 s |
| `restore <directory>` | 590 MB, 0.35 to 0.36 s | 8 MB, 0.37 to 0.39 s |

Restoring now reads the copy twice, once to check it and once to write
it: 0.37 to 0.39 s here where it took 0.35 to 0.36 s. The backup is on the
same SSD as the store, and was read moments before, so the second read
comes from the system's cache: on a disk that has to be read again it
costs a second read of the copy.

What is held now is a part of the copy, one megabyte, and the record
that runs across its end, so a record of many megabytes still takes that
much.

The script was changed after these runs to stop when a measured command
fails, where it printed a row regardless; what it measures and prints
for a command that succeeds is the same.
