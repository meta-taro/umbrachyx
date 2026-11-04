#!/usr/bin/env bash
# GET from many connections at once, for Umbrachyx and Valkey, with
# 1,000,000 keys present and 32-byte values: 1,000, 5,000 and 10,000
# clients, each waiting for its reply before the next request. The client
# runs on several threads so that it is less of a limit than either server.
# Valkey runs with appendfsync always, as in bench/valkey.sh.
#
# Usage: bench/connections.sh [requests]
set -euo pipefail

requests="${1:-500000}"
umbrachyx_port=6390
valkey_port=6399
keys=1000000
client_threads=4
max_clients=20000
port_cooldown=40
dir="$(mktemp -d)"
server=
trap '[ -n "$server" ] && kill "$server" 2>/dev/null; valkey-cli -p "$valkey_port" shutdown nosave >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

fill() {
  valkey-benchmark -p "$1" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
}

measure() {
  local label="$1" port="$2" clients
  for clients in 1000 5000 10000; do
    # Closed connections hold their local port for a while (TIME_WAIT), and
    # 10,000 clients need most of the ports there are; let them free up.
    sleep "$port_cooldown"
    valkey-benchmark -p "$port" -t get -n "$requests" -r "$keys" -d 32 -c "$clients" \
      --threads "$client_threads" --csv 2>"$dir/client.err" \
      | grep -v '"test"' | sed "s/^/\"$label, $clients clients\",/" \
      || echo "\"$label, $clients clients\",\"failed: $(tr '\n' ' ' <"$dir/client.err" | cut -c1-120)\""
  done
}

echo '"server","test","rps","avg_latency_ms","min_latency_ms","p50_latency_ms","p95_latency_ms","p99_latency_ms","max_latency_ms"'

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"
export UMBRACHYX_DB="$dir/log"
"$bin" branch bench --from main >/dev/null
"$bin" serve bench --port "$umbrachyx_port" --max-connections "$max_clients" 2>"$dir/serve.log" &
server=$!
until valkey-cli -p "$umbrachyx_port" ping >/dev/null 2>&1; do sleep 0.1; done
fill "$umbrachyx_port"
measure umbrachyx "$umbrachyx_port"
echo "umbrachyx: $(grep -c 'no thread' "$dir/serve.log" || true) connections without a thread" >&2
kill "$server"
server=

valkey-server --port "$valkey_port" --dir "$dir" --save '' --appendonly yes --appendfsync always \
  --maxclients "$max_clients" --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
until valkey-cli -p "$valkey_port" ping >/dev/null 2>&1; do sleep 0.1; done
fill "$valkey_port"
measure valkey "$valkey_port"
