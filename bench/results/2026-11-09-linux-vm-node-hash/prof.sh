#!/usr/bin/env bash
# Where the server's CPU goes under pipelined SETs and 50-client GETs.
# Usage: prof.sh <binary>
set -u
bin="$1"; d=$(mktemp -d); export UMBRACHYX_DB=$d/log
"$bin" branch bench --from main >/dev/null
"$bin" serve bench --port 6390 2>$d/serve.log & pid=$!
until valkey-cli -p 6390 ping >/dev/null 2>&1; do sleep 0.1; done
valkey-benchmark -p 6390 -t set -n 200000 -r 200000 -d 32 -c 50 -P 64 -q >/dev/null 2>&1
perf record -o $d/perf.data -p $pid -- sleep 14 >$d/perf.log 2>&1 & rec=$!
sleep 1
valkey-benchmark -p 6390 -t set -n 200000 -r 200000 -d 32 -c 50 -P 64 -q 2>&1 | tr '\r' '\n' | tail -2
valkey-benchmark -p 6390 -t get -n 300000 -r 200000 -d 32 -c 50 -q 2>&1 | tr '\r' '\n' | tail -2
wait $rec
echo "== by library"; perf report -i $d/perf.data --no-children --sort dso 2>/dev/null | grep -v '^#' | grep -v '^$' | head -8
echo "== top libc symbols"; perf report -i $d/perf.data --no-children --dso libc.so.6 --sort sym 2>/dev/null | grep -v "^#" | grep -v "^$" | head -12; echo "== top symbols"; perf report -i $d/perf.data --no-children --sort dso,sym 2>/dev/null | grep -v "^#" | grep -v "^$" | head -15
echo "== perf log"; cat $d/perf.log; echo "== serve log"; tail -5 $d/serve.log; ls -la $d; kill $pid; wait $pid 2>/dev/null; rm -rf $d
