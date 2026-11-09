#!/usr/bin/env bash
# One run of the allocator comparison against one built binary.
# Usage: alloc.sh <allocator label> <umbrachyx binary> [sets]
# Prints valkey-benchmark CSV rows prefixed by the allocator and the step,
# then one memory row.
set -euo pipefail

label="$1"
bin="$2"
sets="${3:-200000}"
port=6390
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

rss_kib() { ps -o rss= -p "$1" | tr -d ' '; }
bench() {
  local step="$1"; shift
  valkey-benchmark -p "$port" -r "$sets" -d 32 --csv "$@" 2>/dev/null \
    | grep -v '"test"' | sed "s/^/\"$label\",\"$step\",/"
}

export UMBRACHYX_DB="$dir/log"
"$bin" branch bench --from main >/dev/null
"$bin" serve bench --port "$port" 2>"$dir/serve.log" &
server=$!
until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done

empty="$(rss_kib "$server")"
bench "SET, 50 clients, pipelined 64 (first pass)" -t set -n "$sets" -c 50 -P 64
first="$(rss_kib "$server")"
bench "GET, 50 clients" -t get -n 100000 -c 50
bench "SET, 50 clients" -t set -n 50000 -c 50
bench "SET, 50 clients, pipelined 64 (second pass)" -t set -n "$sets" -c 50 -P 64
second="$(rss_kib "$server")"
echo "\"$label\",\"memory\",\"rss_kib_empty=$empty\",\"rss_kib_first=$first\",\"rss_kib_end=$second\",\"sets_between=$((sets + 50000))\",\"bytes_per_set=$(((second - first) * 1024 / (sets + 50000)))\""
kill "$server"
wait "$server" 2>/dev/null || true
server=
