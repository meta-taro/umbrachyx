#!/usr/bin/env bash
# Umbrachyx's side of the Redis-protocol rows in docs/comparison.md, the
# same runs as bench/valkey.sh: GET and SET from one client over loopback,
# with 32-byte values over a space of 1,000,000 keys, and PING as the round
# trip with no work. Then GET from 50 clients at once. Every SET is a commit,
# durable before it is acknowledged, so there is one run, not two. Last,
# GET from one client while 50 others keep sending SETs.
#
# Then the same GET and SET runs on a fresh server whose keys are in a
# table declared to keep only its latest values (`pos/bench/key:<n>`). Those
# rows say "latest table"; their `test` column is GET or SET, as for the
# others. The keys share one subject (`pos/bench`), as `key:<n>` do, so
# both runs make one encryption key, not one per key.
#
# Usage: bench/umbrachyx.sh [requests]
set -euo pipefail

requests="${1:-100000}"
port=6390
keys=1000000
branch=bench
# A 32-byte value, as -d 32 gives SETs that do not name their key.
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
dir="$(mktemp -d)"
server=
writers=
trap '[ -n "$writers" ] && kill "$writers" 2>/dev/null; [ -n "$server" ] && kill "$server" 2>/dev/null; rm -rf "$dir"' EXIT

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

# Starts a server on a fresh log named <name>; with a table, declared
# latest before anything is written.
start() {
  local name="$1" table="${2:-}"
  export UMBRACHYX_DB="$dir/$name.log"
  if [ -n "$table" ]; then
    "$bin" table "$table" latest --who bench --why "bench/umbrachyx.sh" >/dev/null
  fi
  "$bin" branch "$branch" --from main >/dev/null
  "$bin" serve "$branch" --port "$port" 2>"$dir/$name.serve.log" &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
}

# Stops a background job and reaps it, so the shell reports nothing.
reap() {
  kill "$1"
  wait "$1" 2>/dev/null || true
}

# valkey-benchmark against the server, its CSV rows labelled <label>. With
# a table, the command names its key in that table, and the `test` column
# is cut back to the command's name.
bench() {
  local label="$1" table="$2"
  shift 2
  if [ -z "$table" ]; then
    valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null
  else
    local command="$1"
    shift
    if [ "$command" = SET ]; then
      valkey-benchmark -p "$port" -r "$keys" --csv "$@" SET "$table/bench/key:__rand_int__" "$value" 2>/dev/null
    else
      valkey-benchmark -p "$port" -r "$keys" --csv "$@" GET "$table/bench/key:__rand_int__" 2>/dev/null
    fi | sed -E 's/^"(GET|SET) [^"]*"/"\1"/'
  fi | grep -v '"test"' | sed "s/^/\"$label\",/"
}

echo '"persistence","test","rps","avg_latency_ms","min_latency_ms","p50_latency_ms","p95_latency_ms","p99_latency_ms","max_latency_ms"'

start history
# Fill the key space first, so GET finds values.
valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
bench "every commit synced" "" -t ping_mbulk,get,set -n "$requests" -c 1
bench "every commit synced, 50 clients" "" -t get -n "$requests" -c 50
# More SETs than can finish before the GETs do; stopped once they have.
valkey-benchmark -p "$port" -t set -n 100000000 -r "$keys" -d 32 -c 50 -q >/dev/null 2>&1 &
writers=$!
sleep 1
bench "every commit synced, GET while 50 clients SET" "" -t get -n "$requests" -c 1
reap "$writers"
writers=
reap "$server"
server=

start latest pos
valkey-benchmark -p "$port" -n "$keys" -r "$keys" -c 50 -P 64 -q \
  SET "pos/bench/key:__rand_int__" "$value" >/dev/null 2>&1
bench "every commit synced, latest table" pos GET -n "$requests" -c 1
bench "every commit synced, latest table" pos SET -n "$requests" -c 1
bench "every commit synced, latest table, 50 clients" pos GET -n "$requests" -c 50
valkey-benchmark -p "$port" -n 100000000 -r "$keys" -c 50 -q \
  SET "pos/bench/key:__rand_int__" "$value" >/dev/null 2>&1 &
writers=$!
sleep 1
bench "every commit synced, latest table, GET while 50 clients SET" pos GET -n "$requests" -c 1
reap "$writers"
writers=
reap "$server"
server=
