#!/usr/bin/env bash
# Trial only, not for merging: the writes to a plain-async table from a
# client on the server's own machine, with the serving threads' two
# waiting times switched, beside Valkey with appendfsync everysec. Each
# row says what the client and the server used of the processors, and
# how many of the puts the serving threads held themselves.
#
# Usage: bench/trial.sh <old binary> <new binary> [rounds]
set -uo pipefail
old="$1"
new="$2"
rounds="${3:-3}"
bin=
port=6390
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
tick="$(getconf CLK_TCK)"
dir=
server=

cpu() { # seconds of processor a process and its threads have used
  awk -v tick="$tick" '{print ($14 + $15) / tick}' "/proc/$1/stat"
}
held() {
  [ -f "$dir/err" ] && grep held-early "$dir/err" | tail -1 | cut -d' ' -f2 || true
}

row() { # name label requests args...
  local name="$1" label="$2" requests="$3"
  shift 3
  local before_cpu before_held after_held rps used
  before_cpu="$(cpu "$server")"
  before_held="$(held)"
  # The processor time is the whole pipeline's: all but the client's is
  # too little to show.
  local TIMEFORMAT='%U %S %R'
  { time valkey-benchmark -p "$port" -r 1000000 -n "$requests" --csv "$@" 2>/dev/null |
    grep -v '"test"' | cut -d, -f2,5,7,8 | tr -d '"' >"$dir/rps"; } 2>"$dir/time"
  used="$(awk -v a="$before_cpu" -v b="$(cpu "$server")" 'BEGIN {print b - a}')"
  sleep 1.2
  after_held="$(held)"
  rps="$(cat "$dir/rps")"
  echo "$name,$label,$rps,$(awk '{printf "%.2f,%.2f", ($1 + $2) / $3, $3}' "$dir/time"),$used,$requests,$(( ${after_held:-0} - ${before_held:-0} ))"
}

one() { # name warm threads stay look
  local name="$1" warm="$2" threads="$3" stay="$4" look="$5" key
  dir="$(mktemp -d)"
  if [ "$name" = valkey ] || [ "$name" = valkey-after-50-clients ]; then
    valkey-server --port "$port" --dir "$dir" --save '' --appendonly yes --appendfsync everysec \
      >/dev/null 2>&1 &
    server=$!
    key=key:__rand_int__
  else
    export UMBRACHYX_DB="$dir/log"
    unset UMBRACHYX_TRIAL_STAY_AWAKE_US UMBRACHYX_TRIAL_LOOK_US
    [ "$stay" = - ] || export UMBRACHYX_TRIAL_STAY_AWAKE_US="$stay"
    [ "$look" = - ] || export UMBRACHYX_TRIAL_LOOK_US="$look"
    "$bin" table kv plain-async --who bench --why trial >/dev/null
    "$bin" branch bench --from main >/dev/null
    if [ "$threads" = - ]; then
      "$bin" serve bench --port "$port" 2>"$dir/err" &
    else
      "$bin" serve bench --port "$port" --threads "$threads" 2>"$dir/err" &
    fi
    server=$!
    key=kv/bench/key:__rand_int__
  fi
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  valkey-benchmark -p "$port" -n 1000000 -r 1000000 -c 50 -P 64 -q SET "$key" "$value" >/dev/null 2>&1
  if [ "$warm" = warm ]; then
    # As bench/umbrachyx.sh reaches these rows: after a second of 50
    # clients writing, stopped mid-way.
    valkey-benchmark -p "$port" -n 100000000 -r 1000000 -c 50 -q SET "$key" "$value" >/dev/null 2>&1 &
    local writers=$!
    sleep 1.5
    kill "$writers"
    wait "$writers" 2>/dev/null
  fi
  row "$name" "8 clients" 300000 -c 8 SET "$key" "$value"
  row "$name" "64 clients" 500000 -c 64 SET "$key" "$value"
  row "$name" "256 clients" 500000 -c 256 SET "$key" "$value"
  row "$name" "1 client" 100000 -c 1 SET "$key" "$value"
  row "$name" "50 clients 64 at a time" 2000000 -c 50 -P 64 SET "$key" "$value"
  row "$name" "50 clients 16 at a time GET" 2000000 -c 50 -P 16 GET "$key"
  kill "$server"
  wait "$server" 2>/dev/null
  rm -rf "$dir"
}

nproc
lscpu | grep -E '^(Model name|Thread|Core)'
echo 'server,row,rps,p50_ms,p99_ms,max_ms,client_processors,seconds,server_processor_seconds,requests,held_by_serving_threads'
for _ in $(seq "$rounds"); do
  one valkey cold - - -
  bin="$old"; one "old" cold - - -
  bin="$new"; one "new" cold - - -
  bin="$old"; one "old-1-thread" cold 1 - -
  bin="$new"; one "new-1-thread" cold 1 - -
done
