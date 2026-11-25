#!/usr/bin/env bash
# What checking and restoring a backup cost: the most memory the process
# held and how long it took, for a backup of <sets> SETs of 32-byte values
# over a space of as many keys. `backup <directory> --check` and
# `restore <directory>` are each run three times on the same backup.
#
# Usage: bench/restore.sh [sets]
#
# UMBRACHYX_BIN names a binary to measure instead of the one built here,
# so two versions can be measured on the same machine. The server that
# writes the SETs and takes the backup is always the one built here.
set -euo pipefail

sets="${1:-1000000}"
port=6394
branch=bench
runs=3
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

cargo build --quiet --release -p umbrachyx
built="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"
measured="${UMBRACHYX_BIN:-$built}"

# Runs a command and prints the most memory it held, in MB, and how long
# it took, in seconds. GNU time reports KiB, the BSD one bytes.
cost() {
  local out="$dir/time.out"
  if /usr/bin/time -v true >/dev/null 2>&1; then
    /usr/bin/time -v "$@" >/dev/null 2>"$out"
    awk -F': ' '/Maximum resident set size/ {rss=$2/1024}
                /Elapsed \(wall clock\)/ {n=split($2,t,":"); s=t[n]+60*t[n-1]}
                END {printf "%.0f MB  %.2f s\n", rss, s}' "$out"
  else
    /usr/bin/time -l "$@" >/dev/null 2>"$out"
    awk '/maximum resident set size/ {rss=$1/1048576}
         / real / {s=$1}
         END {printf "%.0f MB  %.2f s\n", rss, s}' "$out"
  fi
}

export UMBRACHYX_DB="$dir/store"
"$built" branch "$branch" --from main >/dev/null
"$built" serve "$branch" --port "$port" 2>/dev/null &
server=$!
until valkey-cli -p "$port" PING >/dev/null 2>&1; do sleep 0.1; done
valkey-benchmark -p "$port" -t set -n "$sets" -r "$sets" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
kill "$server"
wait "$server" 2>/dev/null || true
server=
"$built" backup "$dir/backup" >/dev/null

if [ -n "${UMBRACHYX_BIN:-}" ]; then
  echo "measured: the binary UMBRACHYX_BIN names"
else
  echo "measured: the binary built from this tree"
fi
echo "sets: $sets, copy of the log: $(wc -c <"$dir/backup/log" | tr -d ' ') bytes"
for run in $(seq "$runs"); do
  echo "backup --check  run $run: $(cost "$measured" backup "$dir/backup" --check)"
done
for run in $(seq "$runs"); do
  rm -f "$dir/restored" "$dir/restored.keys"
  echo "restore         run $run: $(UMBRACHYX_DB="$dir/restored" cost "$measured" restore "$dir/backup")"
done
