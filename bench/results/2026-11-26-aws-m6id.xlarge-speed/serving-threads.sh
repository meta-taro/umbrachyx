#!/usr/bin/env bash
# The 64-client SET row again, with what each thread of the server and of the client used.
set -uo pipefail
S=/mnt/nvme; export PATH="$S/valkey/src:$PATH"; bin=$S/new/target/release/umbrachyx
port=6390; value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
one() { # name clients serve-args...
  local name="$1" clients="$2"; shift 2
  local d; d="$(mktemp -d -p $S/tmp)"
  if [ "$name" = valkey ]; then
    valkey-server --port $port --dir "$d" --save '' --appendonly yes --appendfsync everysec >/dev/null 2>&1 &
    key=key:__rand_int__
  else
    export UMBRACHYX_DB="$d/log"
    $bin table kv plain-async --who bench --why r64 >/dev/null
    $bin branch bench --from main >/dev/null
    $bin serve bench --port $port "$@" 2>"$d/err" &
    key=kv/bench/key:__rand_int__
  fi
  local server=$!
  until valkey-cli -p $port ping >/dev/null 2>&1; do sleep 0.1; done
  valkey-benchmark -p $port -n 1000000 -r 1000000 -c 50 -P 64 -q SET $key $value >/dev/null 2>&1
  valkey-benchmark -p $port -n 3000000 -r 1000000 -c "$clients" --csv SET $key $value 2>/dev/null | grep -v '"test"' | cut -d, -f2 > "$d/rps" &
  local client=$!
  sleep 3
  local before; before="$(cat /proc/$server/task/*/status | awk '/ctxt_switches/ {s+=$2} END {print s}')"
  top -H -b -d 5 -n 2 -p "$server" -p "$client" | awk '/^top/ {n++} n==2 && $1 ~ /^[0-9]+$/ && $9 > 1 {printf "    %-16s %5.1f%%\n", $12 $13, $9}' > "$d/top"
  local after; after="$(cat /proc/$server/task/*/status | awk '/ctxt_switches/ {s+=$2} END {print s}')"
  wait $client
  echo "$name $* | $clients clients | $(cat "$d/rps") a second | server context switches a second: $(( (after - before) / 5 ))"
  cat "$d/top"
  kill $server; wait $server 2>/dev/null; rm -rf "$d"
}
for round in 1 2; do
  one valkey 64
  one umbrachyx 64
  one umbrachyx 64 --threads 1
  one umbrachyx 64 --threads 2
  one umbrachyx 64 --threads 4
done
one valkey 8; one umbrachyx 8; one valkey 256; one umbrachyx 256
echo R64-DONE
