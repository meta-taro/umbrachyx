#!/usr/bin/env bash
# Memory held by each server for the same writes: the resident size of the
# process after <sets> SETs of 32-byte values over a space of as many keys,
# then after the same number again over the same space. Valkey keeps the
# latest value of each key, so the second pass adds little. Umbrachyx keeps
# every SET as a commit, so it grows with every SET; the last column is that
# growth per SET. It is measured twice: with keys in no table, which keep
# their history, and with keys in a table declared to keep only its latest
# values (`pos/bench/key:<n>`). Those keys share one subject, as `key:<n>`
# do, so neither run makes an encryption key per key.
#
# The default is smaller than the other scripts' 1,000,000: two passes of a
# million would need about 16 GB for Umbrachyx, as much as the machines
# these runs are on.
#
# Usage: bench/memory.sh [sets]
set -euo pipefail

sets="${1:-200000}"
umbrachyx_port=6390
valkey_port=6399
branch=bench
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; valkey-cli -p "$valkey_port" shutdown nosave >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

# Resident size in KiB, as ps reports it on Linux and macOS.
rss_kib() {
  ps -o rss= -p "$1" | tr -d ' '
}

# A 32-byte value, as -d 32 gives SETs that do not name their key.
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

# SETs to <port>, of keys `key:<n>` or, given a table,
# `<table>/bench/key:<n>`.
fill() {
  if [ -z "${2:-}" ]; then
    valkey-benchmark -p "$1" -t set -n "$sets" -r "$sets" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
  else
    valkey-benchmark -p "$1" -n "$sets" -r "$sets" -c 50 -P 64 -q \
      SET "$2/bench/key:__rand_int__" "$value" >/dev/null 2>&1
  fi
}

# Prints one row for the server listening on <port> as <pid>, writing to
# <table> if one is given.
measure() {
  local name="$1" port="$2" pid="$3" table="${4:-}" before first second
  before="$(rss_kib "$pid")"
  fill "$port" "$table"
  first="$(rss_kib "$pid")"
  fill "$port" "$table"
  second="$(rss_kib "$pid")"
  echo "\"$name\",\"$sets\",\"$before\",\"$first\",\"$((sets * 2))\",\"$second\",\"$(((second - first) * 1024 / sets))\""
}

echo '"server","sets","rss_kib_empty","rss_kib","sets_twice","rss_kib_twice","second_pass_bytes_per_set"'

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

# One Umbrachyx server on a fresh log; with a table, declared latest first.
umbrachyx() {
  local name="$1" table="${2:-}"
  export UMBRACHYX_DB="$dir/$name.log"
  if [ -n "$table" ]; then
    "$bin" table "$table" latest --who bench --why "bench/memory.sh" >/dev/null
  fi
  "$bin" branch "$branch" --from main >/dev/null
  "$bin" serve "$branch" --port "$umbrachyx_port" 2>"$dir/$name.serve.log" &
  server=$!
  until valkey-cli -p "$umbrachyx_port" ping >/dev/null 2>&1; do sleep 0.1; done
  measure "$name" "$umbrachyx_port" "$server" "$table"
  kill "$server"
  # Reaped here so the shell does not report the stopped job.
  wait "$server" 2>/dev/null || true
  server=
}

umbrachyx umbrachyx
umbrachyx "umbrachyx (latest table)" pos

mkdir "$dir/valkey"
valkey-server --port "$valkey_port" --dir "$dir/valkey" --save '' \
  --appendonly yes --appendfsync always \
  --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
until valkey-cli -p "$valkey_port" ping >/dev/null 2>&1; do sleep 0.1; done
measure "valkey (appendfsync always)" "$valkey_port" "$(cat "$dir/valkey.pid")"
