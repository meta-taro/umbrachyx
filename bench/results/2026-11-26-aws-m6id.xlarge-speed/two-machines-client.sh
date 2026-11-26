#!/usr/bin/env bash
# A plain-async table and Valkey everysec on another machine (4 vCPUs), driven from this one (8 vCPUs) over the private network.
set -uo pipefail
export PATH="/mnt/nvme/valkey/src:$PATH"
A=SERVER_PRIVATE_ADDRESS; value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
until grep -q CLIENT-DONE ~/client.out; do sleep 5; done
row() { # server label args...
  local server="$1" label="$2"; shift 2
  echo "\"$server\",\"$label\",$(valkey-benchmark -h $A -p $port -r 1000000 --csv "$@" 2>/dev/null | grep -v '"test"' | cut -d, -f1,2,6,8)"
}
echo '"server","clients","test","rps","p50_latency_ms","p99_latency_ms"'
for round in 1 2 3; do
  for s in "umbrachyx, 2 serving threads:6390:kv/bench/key:__rand_int__" "umbrachyx, 1 serving thread:6391:kv/bench/key:__rand_int__" "umbrachyx, 4 serving threads:6392:kv/bench/key:__rand_int__" "valkey, appendfsync everysec:6399:key:__rand_int__"; do
    name="${s%%:*}"; rest="${s#*:}"; port="${rest%%:*}"; key="${rest#*:}"
    valkey-benchmark -h $A -p $port -n 1000000 -r 1000000 -c 50 -P 64 -q SET $key $value >/dev/null 2>&1
    row "$name" "1 client" -n 100000 -c 1 SET $key $value
    row "$name" "8 clients" -n 500000 -c 8 SET $key $value
    row "$name" "64 clients" -n 1000000 -c 64 SET $key $value
    row "$name" "64 clients, client of 4 threads" -n 2000000 -c 64 --threads 4 SET $key $value
    row "$name" "256 clients, client of 4 threads" -n 2000000 -c 256 --threads 4 SET $key $value
    row "$name" "50 clients, 64 at a time, client of 4 threads" -n 8000000 -c 50 -P 64 --threads 4 SET $key $value
    row "$name" "50 clients, 16 at a time, client of 4 threads" -n 8000000 -c 50 -P 16 --threads 4 GET $key
  done
done
echo NET-DONE
