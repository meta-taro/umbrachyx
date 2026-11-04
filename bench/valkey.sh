#!/usr/bin/env bash
# Valkey's side of docs/comparison.md: GET and SET latency from one client
# over loopback, with 32-byte values over a space of 1,000,000 keys, first
# without persistence, then with every write fsynced before it is
# acknowledged (appendfsync always). PING is the round trip with no work.
# Then SET with appendfsync always from 8, 64 and 256 clients at once, each
# waiting for its reply before the next request, GET from 50 clients, and
# GET from one client while 50 others keep sending SETs.
#
# Usage: bench/valkey.sh [requests] [concurrent-requests]
set -euo pipefail

requests="${1:-100000}"
concurrent_requests="${2:-20000}"
port=6399
keys=1000000
dir="$(mktemp -d)"
writers=
trap '[ -n "$writers" ] && kill "$writers" 2>/dev/null; valkey-cli -p "$port" shutdown nosave >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

start() {
  local appendonly="$1" data
  # A fresh directory each time, so no earlier append-only file is loaded.
  data="$(mktemp -d "$dir/run.XXXX")"
  valkey-server --port "$port" --dir "$data" --save '' \
    --appendonly "$appendonly" --appendfsync always \
    --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  # Fill the key space first, so GET finds values.
  valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null
}

stop() {
  valkey-cli -p "$port" shutdown nosave >/dev/null 2>&1 || true
  while [ -e "$dir/valkey.pid" ]; do sleep 0.1; done
}

run() {
  local label="$1" appendonly="$2"
  start "$appendonly"
  valkey-benchmark -p "$port" -t ping_mbulk,get,set -n "$requests" -r "$keys" -d 32 -c 1 --csv \
    | grep -v '"test"' | sed "s/^/\"$label\",/"
  stop
}

concurrent() {
  start yes
  for clients in 8 64 256; do
    valkey-benchmark -p "$port" -t set -n "$concurrent_requests" -r "$keys" -d 32 -c "$clients" --csv \
      | grep -v '"test"' | sed "s/^/\"appendfsync always, $clients clients\",/"
  done
  valkey-benchmark -p "$port" -t get -n "$requests" -r "$keys" -d 32 -c 50 --csv \
    | grep -v '"test"' | sed 's/^/"appendfsync always, 50 clients",/'
  # More SETs than can finish before the GETs do; stopped once they have.
  valkey-benchmark -p "$port" -t set -n 100000000 -r "$keys" -d 32 -c 50 -q >/dev/null 2>&1 &
  writers=$!
  sleep 1
  valkey-benchmark -p "$port" -t get -n "$requests" -r "$keys" -d 32 -c 1 --csv \
    | grep -v '"test"' | sed 's/^/"appendfsync always, GET while 50 clients SET",/'
  kill "$writers"
  writers=
  stop
}

echo '"persistence","test","rps","avg_latency_ms","min_latency_ms","p50_latency_ms","p95_latency_ms","p99_latency_ms","max_latency_ms"'
run "none" no
run "appendfsync always" yes
concurrent
