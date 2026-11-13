#!/usr/bin/env bash
# Two builds of the server measured in turn on one machine, to tell
# whether a change is faster there: what one machine gains, another may
# not. Each round starts each build on a fresh log, fills a million keys
# and runs the rows that many writers and many readers decide.
#
# Both checkouts must already be built (`cargo build --release -p
# umbrachyx`), and `valkey-benchmark` and `valkey-cli` on the PATH. The
# logs go to the temporary directory: point TMPDIR at the disk to measure.
#
# Usage: bench/ab.sh <old-checkout> <new-checkout> [rounds]
set -euo pipefail

old="$1"
new="$2"
rounds="${3:-3}"
port=6390
keys=1000000
server=
dir=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; [ -n "$dir" ] && rm -rf "$dir"' EXIT

row() {
  local side="$1" label="$2"
  shift 2
  valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null | grep -v '"test"' |
    python3 -c 'import csv, sys
for r in csv.reader(sys.stdin):
    print(",".join([sys.argv[1], "\"" + sys.argv[2] + "\"", r[0], r[1], r[4], r[6]]))' "$side" "$label"
}

echo 'side,clients,test,rps,p50_latency_ms,p99_latency_ms'
for _ in $(seq 1 "$rounds"); do
  for side in old new; do
    checkout="$old"
    [ "$side" = new ] && checkout="$new"
    dir="$(mktemp -d)"
    export UMBRACHYX_DB="$dir/log"
    bin="$checkout/target/release/umbrachyx"
    "$bin" branch bench --from main >/dev/null
    "$bin" serve bench --port "$port" 2>/dev/null &
    server=$!
    until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
    valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
    row "$side" "50 clients, 64 at a time" -t set -n 2000000 -c 50 -P 64
    row "$side" "64 clients" -t set -n 200000 -c 64
    row "$side" "256 clients" -t set -n 200000 -c 256
    row "$side" "1 client" -t set,get -n 100000 -c 1
    row "$side" "50 clients" -t get -n 200000 -c 50
    row "$side" "50 clients, 16 at a time" -t get -n 2000000 -c 50 -P 16
    echo "$side,\"resident KiB after 3.5 million SETs\",,$(ps -o rss= -p "$server" | tr -d ' '),,"
    kill "$server"
    wait "$server" 2>/dev/null || true
    server=
    rm -rf "$dir"
    dir=
  done
done
