#!/usr/bin/env bash
# Valkey's side of docs/comparison.md: GET and SET latency from one client
# over loopback, with 32-byte values over a space of 1,000,000 keys, first
# without persistence, then with every write fsynced before it is
# acknowledged (appendfsync always). PING is the round trip with no work.
#
# Usage: bench/valkey.sh [requests]
set -euo pipefail

requests="${1:-100000}"
port=6399
keys=1000000
dir="$(mktemp -d)"
trap 'valkey-cli -p "$port" shutdown nosave >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

run() {
  local label="$1" appendonly="$2"
  valkey-server --port "$port" --dir "$dir" --save '' \
    --appendonly "$appendonly" --appendfsync always \
    --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  # Fill the key space first, so GET finds values.
  valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null
  valkey-benchmark -p "$port" -t ping_mbulk,get,set -n "$requests" -r "$keys" -d 32 -c 1 --csv \
    | grep -v '"test"' | sed "s/^/\"$label\",/"
  valkey-cli -p "$port" shutdown nosave >/dev/null 2>&1 || true
  while [ -e "$dir/valkey.pid" ]; do sleep 0.1; done
}

echo '"persistence","test","rps","avg_latency_ms","min_latency_ms","p50_latency_ms","p95_latency_ms","p99_latency_ms","max_latency_ms"'
run "none" no
run "appendfsync always" yes
