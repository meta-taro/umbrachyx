#!/usr/bin/env bash
# How the number of serving threads changes what the server does on this
# machine: the rows of bench/umbrachyx.sh that many clients decide, run
# against a server started with 1, 2 and 4 serving threads (one more
# thread writes, whatever the number). On a machine whose virtual CPUs are
# two to a core, the threads and the benchmark's own client share few
# cores, and more threads is not always more work done.
#
# With `perf` installed it then says where the server's time goes while
# 50 clients send writes 64 at a time: by thread, and by function in the
# thread that writes. Without it, that part is skipped.
#
# Usage: bench/threads.sh [requests]
set -euo pipefail

requests="${1:-200000}"
pipelined_requests=$((requests * 10))
port=6390
keys=1000000
branch=bench
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

# Starts a server with <threads> serving threads on a fresh log.
start() {
  local threads="$1" name="${2:-threads-$1}"
  export UMBRACHYX_DB="$dir/$name.log"
  "$bin" branch "$branch" --from main >/dev/null
  "$bin" serve "$branch" --port "$port" --threads "$threads" 2>/dev/null &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  # Fill the key space first, so reads find values and writes overwrite.
  valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
}

stop() {
  kill "$server"
  wait "$server" 2>/dev/null || true
  server=
}

row() {
  local threads="$1" label="$2"
  shift 2
  valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null | grep -v '"test"' |
    python3 -c 'import csv, sys
for r in csv.reader(sys.stdin):
    print(",".join([sys.argv[1], "\"" + sys.argv[2] + "\"", r[0], r[1], r[4], r[6]]))' "$threads" "$label"
}

echo 'threads,clients,test,rps,p50_latency_ms,p99_latency_ms'
for threads in 1 2 4; do
  start "$threads"
  row "$threads" "1 client" -t set,get -n "$requests" -c 1
  row "$threads" "50 clients" -t get -n "$requests" -c 50
  row "$threads" "64 clients" -t set -n "$requests" -c 64
  row "$threads" "256 clients" -t set -n "$requests" -c 256
  row "$threads" "50 clients, 16 at a time" -t get -n "$pipelined_requests" -c 50 -P 16
  row "$threads" "50 clients, 64 at a time" -t set -n "$pipelined_requests" -c 50 -P 64
  stop
done

if ! command -v perf >/dev/null; then
  echo "perf is not installed: no profile" >&2
  exit 0
fi

start 2 profile
valkey-benchmark -p "$port" -t set -n 100000000 -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1 &
load=$!
sleep 3
perf record -F 999 -g -p "$server" -o "$dir/perf.data" -- sleep 10 >/dev/null 2>&1 || true
kill "$load" 2>/dev/null || true
wait "$load" 2>/dev/null || true
echo
echo "# 2 serving threads, 50 clients sending writes 64 at a time: time by thread"
perf report -i "$dir/perf.data" --no-children -g none --sort comm 2>/dev/null |
  grep -v '^#' | grep '%' | cut -c1-70 | head -8
echo "# the same: time by thread and by where the code is"
perf report -i "$dir/perf.data" --no-children -g none --sort comm,dso 2>/dev/null |
  grep -v '^#' | grep '%' | cut -c1-90 | head -12
echo "# the same: the thread that writes, by function, with what each calls"
perf report -i "$dir/perf.data" --children -g none --comm "resp commits" --sort symbol 2>/dev/null |
  grep -v '^#' | grep '%' | cut -c1-140 | head -45
stop
