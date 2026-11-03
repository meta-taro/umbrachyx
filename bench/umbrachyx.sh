#!/usr/bin/env bash
# Umbrachyx's side of the Redis-protocol rows in docs/comparison.md, the
# same runs as bench/valkey.sh: GET and SET from one client over loopback,
# with 32-byte values over a space of 1,000,000 keys, and PING as the round
# trip with no work. Then GET from 50 clients at once. Every SET is a commit,
# durable before it is acknowledged, so there is one run, not two.
#
# Usage: bench/umbrachyx.sh [requests]
set -euo pipefail

requests="${1:-100000}"
port=6390
keys=1000000
branch=bench
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"
export UMBRACHYX_DB="$dir/log"
"$bin" branch "$branch" --from main >/dev/null
"$bin" serve "$branch" --port "$port" 2>"$dir/serve.log" &
server=$!
until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
# Fill the key space first, so GET finds values. This takes minutes: the
# writes pipelined on one connection still wait for their syncs one by one.
valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1

echo '"persistence","test","rps","avg_latency_ms","min_latency_ms","p50_latency_ms","p95_latency_ms","p99_latency_ms","max_latency_ms"'
valkey-benchmark -p "$port" -t ping_mbulk,get,set -n "$requests" -r "$keys" -d 32 -c 1 --csv 2>/dev/null \
  | grep -v '"test"' | sed 's/^/"every commit synced",/'
valkey-benchmark -p "$port" -t get -n "$requests" -r "$keys" -d 32 -c 50 --csv 2>/dev/null \
  | grep -v '"test"' | sed 's/^/"every commit synced, 50 clients",/'
