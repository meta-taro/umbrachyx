#!/usr/bin/env bash
# The allocator's huge pages against its defaults, in turn, three rounds:
# the store's own cost per write, the server's pipelined writes, GETs from
# 50 clients, and the memory the server holds afterwards.
set -euo pipefail
scratch=/mnt/nvme
export PATH="$HOME/.cargo/bin:$scratch/valkey/src:$PATH"
cd ~/umbrachyx
cargo build -q --release -p umbrachyx --example commit_cost
bin=target/release/umbrachyx; cost=target/release/examples/commit_cost
port=6391; keys=1000000
echo "transparent huge pages: $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
echo "commit: $(git rev-parse HEAD)"
with() { if [ "$1" = default ]; then shift; "$@"; else local c="$1"; shift; _RJEM_MALLOC_CONF="$c" "$@"; fi; }
rate() { valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null | grep -v '"test"' | cut -d, -f2 | tr -d '"'; }
for round in 1 2 3; do
  for conf in default thp:always,metadata_thp:always; do
    echo "== round $round, $conf"
    with "$conf" "$cost" "$keys" "$keys" plain 1
    with "$conf" "$cost" "$keys" "$keys" plain 64
    dir="$(mktemp -d -p "$scratch")"; export UMBRACHYX_DB="$dir/log"
    "$bin" branch bench --from main >/dev/null
    e=UNUSED=1; [ "$conf" = default ] || e="_RJEM_MALLOC_CONF=$conf"; env "$e" "$bin" serve bench --port "$port" 2>/dev/null &
    server=$!
    until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
    echo "fill, 50 clients 64 at a time, SET/s: $(rate -t set -n "$keys" -c 50 -P 64)"
    echo "again, 50 clients 64 at a time, SET/s: $(rate -t set -n "$keys" -c 50 -P 64)"
    echo "50 clients, GET/s: $(rate -t get -n 1000000 -c 50)"
    echo "1 client, GET/s: $(rate -t get -n 200000 -c 1)"
    echo "1 client, SET/s: $(rate -t set -n 30000 -c 1)"
    grep -E 'VmRSS|AnonHugePages' "/proc/$server/status" /dev/null | tr -s ' \t' ' ' || true
    grep AnonHugePages "/proc/$server/smaps_rollup" | tr -s ' ' || true
    kill "$server"; wait "$server" 2>/dev/null || true; rm -rf "$dir"
  done
done
echo HUGE-DONE
