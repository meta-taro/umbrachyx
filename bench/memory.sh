#!/usr/bin/env bash
# Memory held by each server for the same writes: the resident size of the
# process after <sets> SETs of 32-byte values over a space of as many keys,
# then after the same number again over the same space. Valkey keeps the
# latest value of each key, so the second pass adds little. Umbrachyx keeps
# every commit and each commit's tree, so it grows with every SET; the last
# column is that growth per SET.
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

fill() {
  valkey-benchmark -p "$1" -t set -n "$sets" -r "$sets" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
}

# Prints one row for the server listening on <port> as <pid>.
measure() {
  local name="$1" port="$2" pid="$3" before first second
  before="$(rss_kib "$pid")"
  fill "$port"
  first="$(rss_kib "$pid")"
  fill "$port"
  second="$(rss_kib "$pid")"
  echo "\"$name\",\"$sets\",\"$before\",\"$first\",\"$((sets * 2))\",\"$second\",\"$(((second - first) * 1024 / sets))\""
}

echo '"server","sets","rss_kib_empty","rss_kib","sets_twice","rss_kib_twice","second_pass_bytes_per_set"'

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"
export UMBRACHYX_DB="$dir/log"
"$bin" branch "$branch" --from main >/dev/null
"$bin" serve "$branch" --port "$umbrachyx_port" 2>"$dir/serve.log" &
server=$!
until valkey-cli -p "$umbrachyx_port" ping >/dev/null 2>&1; do sleep 0.1; done
measure umbrachyx "$umbrachyx_port" "$server"
kill "$server"
# Reaped here so the shell does not report the stopped job.
wait "$server" 2>/dev/null || true
server=

mkdir "$dir/valkey"
valkey-server --port "$valkey_port" --dir "$dir/valkey" --save '' \
  --appendonly yes --appendfsync always \
  --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
until valkey-cli -p "$valkey_port" ping >/dev/null 2>&1; do sleep 0.1; done
measure "valkey (appendfsync always)" "$valkey_port" "$(cat "$dir/valkey.pid")"
