#!/usr/bin/env bash
# Reads and writes mixed at fixed ratios, the same client against every
# server: 50 clients, each sending one command and waiting for its reply,
# a GET 95, 80 or 50 times in 100 and a SET of a 32-byte value otherwise,
# over a space of 1,000,000 keys filled first, for <seconds> each. The
# client is the mixed_load example of this repository.
#
# Umbrachyx is measured three times, with the keys in a table that keeps
# its history, in a `plain` table (no history, every SET on disk before
# its answer) and in a `plain-async` table (answered at once, on disk
# within a second). Valkey is measured with appendfsync always and with
# appendfsync everysec. A row is to be read beside the row that promises
# the same after a power cut: history and plain beside always, plain-async
# beside everysec.
#
# Usage: bench/mixed.sh [seconds]
set -euo pipefail

seconds="${1:-10}"
clients=50
keys=1000000
umbrachyx_port=6390
valkey_port=6399
branch=bench
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; valkey-cli -p "$valkey_port" shutdown nosave >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

cargo build --quiet --release -p umbrachyx --bin umbrachyx --example mixed_load
target="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release"
bin="$target/umbrachyx"
load="$target/examples/mixed_load"

# Every key written once, then the three ratios, against <port>.
rows() {
  local label="$1" port="$2" prefix="$3"
  valkey-benchmark -p "$port" -n "$keys" -r "$keys" -c 50 -P 64 -q \
    SET "${prefix}__rand_int__" "$value" >/dev/null 2>&1
  # Not recorded: the first run after the fill, which was sent 64 at a
  # time, was slower and less even than the same run repeated.
  "$load" warm-up "$port" "$clients" 3 80 "$prefix" >/dev/null
  for reads in 95 80 50; do
    "$load" "$label" "$port" "$clients" "$seconds" "$reads" "$prefix"
  done
}

# One Umbrachyx server on a fresh log; with an engine, the keys are in a
# table declared with it.
umbrachyx() {
  local label="$1" engine="${2:-}" prefix=key:
  export UMBRACHYX_DB="$dir/${engine:-history}.log"
  if [ -n "$engine" ]; then
    "$bin" table kv "$engine" --who bench --why "bench/mixed.sh" >/dev/null
    prefix=kv/bench/key:
  fi
  "$bin" branch "$branch" --from main >/dev/null
  "$bin" serve "$branch" --port "$umbrachyx_port" 2>"$dir/${engine:-history}.serve.log" &
  server=$!
  until valkey-cli -p "$umbrachyx_port" ping >/dev/null 2>&1; do sleep 0.1; done
  rows "$label" "$umbrachyx_port" "$prefix"
  kill "$server"
  # Reaped here so the shell does not report the stopped job.
  wait "$server" 2>/dev/null || true
  server=
}

valkey() {
  local appendfsync="$1" data
  data="$(mktemp -d "$dir/valkey.XXXX")"
  valkey-server --port "$valkey_port" --dir "$data" --save '' \
    --appendonly yes --appendfsync "$appendfsync" \
    --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
  until valkey-cli -p "$valkey_port" ping >/dev/null 2>&1; do sleep 0.1; done
  rows "valkey, appendfsync $appendfsync" "$valkey_port" key:
  valkey-cli -p "$valkey_port" shutdown nosave >/dev/null 2>&1 || true
  while [ -e "$dir/valkey.pid" ]; do sleep 0.1; done
}

echo '"server","clients","reads_percent","ops_per_second","read_p50_ms","read_p99_ms","write_p50_ms","write_p99_ms"'
umbrachyx "umbrachyx, history, every commit synced"
umbrachyx "umbrachyx, plain table, every put synced" plain
umbrachyx "umbrachyx, plain-async table, synced within 1 s" plain-async
valkey always
valkey everysec
