#!/usr/bin/env bash
# How long Umbrachyx takes to start again, and how large its log is,
# after <writes> SETs of 32-byte values over a space of 1,000,000 keys:
# once with the keys in a table that keeps its history and once in a
# `plain` table. The log keeps every write of both, so a restart reads
# them all, also the values a plain table has since overwritten.
#
# The time is that of a command that opens the log and prints the tables,
# from start to exit.
#
# Usage: bench/restart.sh [writes]
set -euo pipefail

writes="${1:-5000000}"
port=6390
keys=1000000
branch=bench
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

now() {
  perl -MTime::HiRes=time -e 'printf "%.3f\n", time'
}

measure() {
  local name="$1" engine="${2:-}" prefix=key: began ended
  export UMBRACHYX_DB="$dir/$name.log"
  if [ -n "$engine" ]; then
    "$bin" table kv "$engine" --who bench --why "bench/restart.sh" >/dev/null
    prefix=kv/bench/key:
  fi
  "$bin" branch "$branch" --from main >/dev/null
  "$bin" serve "$branch" --port "$port" 2>"$dir/$name.serve.log" &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  valkey-benchmark -p "$port" -n "$writes" -r "$keys" -c 50 -P 64 -q \
    SET "${prefix}__rand_int__" "$value" >/dev/null 2>&1
  kill "$server"
  wait "$server" 2>/dev/null || true
  server=
  began="$(now)"
  "$bin" tables >/dev/null
  ended="$(now)"
  echo "\"$name\",\"$writes\",\"$(wc -c <"$UMBRACHYX_DB" | tr -d ' ')\",\"$(perl -e "printf '%.2f', $ended - $began")\""
}

echo '"table","writes","log_bytes","open_seconds"'
measure history
measure plain plain
