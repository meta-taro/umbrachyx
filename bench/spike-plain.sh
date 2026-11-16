#!/usr/bin/env bash
# SPIKE, not a release measurement: how far the cheapest write of a table
# without history gets, before any of it is built properly. The server
# takes the table's name from UMBRACHYX_SPIKE_PLAIN; with
# UMBRACHYX_SPIKE_NOWAIT=<ms> it answers before the sync and syncs what
# it answered at most that often. Valkey runs the same rows with
# `appendfsync always`, `everysec` and no append-only file.
#
# Usage: bench/spike-plain.sh <scratch> [rounds]
set -euo pipefail

scratch="$1"
rounds="${2:-2}"
valkey_version=9.1.3
requests=100000
concurrent_requests=200000
pipelined_requests=$((requests * 20))
keys=1000000
port=6390
vport=6399
branch=bench
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
export TMPDIR="$scratch"
dir="$(mktemp -d)"
server=
writers=
trap '[ -n "$writers" ] && kill "$writers" 2>/dev/null; [ -n "$server" ] && kill "$server" 2>/dev/null; valkey-cli -p "$vport" shutdown nosave >/dev/null 2>&1 || true' EXIT

if [ "$(uname)" = Linux ] && ! command -v valkey-server >/dev/null; then
  sudo apt-get update -qq
  sudo apt-get install -y -qq build-essential git pkg-config curl >/dev/null
  if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
  fi
  valkey="$scratch/valkey"
  if [ ! -x "$valkey/src/valkey-server" ]; then
    git clone --quiet --depth 1 --branch "$valkey_version" https://github.com/valkey-io/valkey.git "$valkey"
    make -C "$valkey" -j"$(nproc)" BUILD_TLS=no >/dev/null
  fi
  export PATH="$valkey/src:$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

reap() {
  kill "$1"
  wait "$1" 2>/dev/null || true
}

# One labelled row: <label> <port> then valkey-benchmark's options and the
# command. The `test` column is cut back to the command's name.
row() {
  local label="$1" p="$2"
  shift 2
  valkey-benchmark -p "$p" -r "$keys" --csv "$@" 2>/dev/null |
    sed -E 's/^"(GET|SET) [^"]*"/"\1"/' | grep -v '"test"' | sed "s/^/\"$label\",/"
}

# Every row against the server on <port>, its keys under <prefix>.
rows() {
  local label="$1" p="$2" prefix="$3"
  valkey-benchmark -p "$p" -n "$keys" -r "$keys" -c 50 -P 64 -q \
    SET "${prefix}key:__rand_int__" "$value" >/dev/null 2>&1
  row "$label, 1 client" "$p" -n "$requests" -c 1 SET "${prefix}key:__rand_int__" "$value"
  row "$label, 1 client" "$p" -n "$requests" -c 1 GET "${prefix}key:__rand_int__"
  row "$label, 50 clients" "$p" -n "$requests" -c 50 GET "${prefix}key:__rand_int__"
  valkey-benchmark -p "$p" -n 100000000 -r "$keys" -c 50 -q \
    SET "${prefix}key:__rand_int__" "$value" >/dev/null 2>&1 &
  writers=$!
  sleep 1
  row "$label, GET while 50 clients SET" "$p" -n "$requests" -c 1 GET "${prefix}key:__rand_int__"
  reap "$writers"
  writers=
  for clients in 8 64 256; do
    row "$label, $clients clients" "$p" -n "$concurrent_requests" -c "$clients" SET "${prefix}key:__rand_int__" "$value"
  done
  row "$label, 50 clients, 16 at a time" "$p" -n "$pipelined_requests" -c 50 -P 16 GET "${prefix}key:__rand_int__"
  row "$label, 50 clients, 64 at a time" "$p" -n "$pipelined_requests" -c 50 -P 64 SET "${prefix}key:__rand_int__" "$value"
}

# <label> <table or ""> <nowait ms or "">
umbrachyx() {
  local label="$1" table="$2" nowait="$3"
  export UMBRACHYX_DB="$dir/$RANDOM.log"
  "$bin" branch "$branch" --from main >/dev/null
  # shellcheck disable=SC2086
  env ${table:+UMBRACHYX_SPIKE_PLAIN=$table} ${nowait:+UMBRACHYX_SPIKE_NOWAIT=$nowait} \
    "$bin" serve "$branch" --port "$port" 2>/dev/null &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  rows "$label" "$port" "${table:-hist}/bench/"
  reap "$server"
  server=
  rm -f "$UMBRACHYX_DB" "$UMBRACHYX_DB.keys"
}

# <label> then valkey-server's persistence options
valkey_rows() {
  local label="$1" data
  shift
  data="$(mktemp -d "$dir/valkey.XXXX")"
  valkey-server --port "$vport" --dir "$data" --save '' "$@" \
    --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
  until valkey-cli -p "$vport" ping >/dev/null 2>&1; do sleep 0.1; done
  rows "$label" "$vport" "kv/bench/"
  valkey-cli -p "$vport" shutdown nosave >/dev/null 2>&1 || true
  while [ -e "$dir/valkey.pid" ]; do sleep 0.1; done
  rm -rf "$data"
}

# The rows an agent's writes take: 50 clients, 64 at a time, with a
# capability token, to a table with history and to one without.
token_rows() {
  local label="$1" table="$2" nowait="$3" public_key
  export UMBRACHYX_DB="$dir/$RANDOM.log"
  rm -f "$dir/root.key"
  public_key="$("$bin" keygen "$dir/root.key")"
  "$bin" token issue --key "$dir/root.key" --subject agent:bench --branch "$branch" \
    --prefix auth/ --columns v --operations read,write --purpose bench --expires-in 8h \
    >"$dir/token"
  "$bin" branch "$branch" --from main >/dev/null
  # shellcheck disable=SC2086
  env ${table:+UMBRACHYX_SPIKE_PLAIN=$table} ${nowait:+UMBRACHYX_SPIKE_NOWAIT=$nowait} \
    "$bin" serve "$branch" --port "$port" --public-key "$public_key" 2>/dev/null &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  valkey-benchmark -p "$port" -r 100000 -n 2000000 -c 50 -P 64 -q \
    SET "auth/__rand_int__/v" "$value" >/dev/null 2>&1
  valkey-benchmark -p "$port" -r 100000 --csv -n "$pipelined_requests" -c 50 -P 64 \
    SET "auth/__rand_int__/v" "$value" 2>/dev/null |
    sed -E 's/^"SET [^"]*"/"SET"/' | grep -v '"test"' | sed "s/^/\"$label, no token, 50 clients, 64 at a time\",/"
  valkey-benchmark -p "$port" -r 100000 --csv -n "$pipelined_requests" -c 50 -P 64 -a "$(cat "$dir/token")" \
    SET "auth/__rand_int__/v" "$value" 2>/dev/null |
    sed -E 's/^"SET [^"]*"/"SET"/' | grep -v '"test"' | sed "s/^/\"$label, token, 50 clients, 64 at a time\",/"
  reap "$server"
  server=
  rm -f "$UMBRACHYX_DB" "$UMBRACHYX_DB.keys"
}

echo "commit: $(git rev-parse HEAD)"
[ -e /sys/kernel/mm/transparent_hugepage/enabled ] &&
  echo "transparent huge pages: $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
echo '"server","test","rps","avg_latency_ms","min_latency_ms","p50_latency_ms","p95_latency_ms","p99_latency_ms","max_latency_ms"'
for round in $(seq "$rounds"); do
  echo "# round $round"
  umbrachyx "umbrachyx history" "" ""
  umbrachyx "umbrachyx plain, synced" kv ""
  umbrachyx "umbrachyx plain, answered first, synced every 10 ms" kv 10
  umbrachyx "umbrachyx plain, answered first, synced every 1000 ms" kv 1000
  valkey_rows "valkey appendfsync always" --appendonly yes --appendfsync always
  valkey_rows "valkey appendfsync everysec" --appendonly yes --appendfsync everysec
  valkey_rows "valkey no append-only file" --appendonly no
  token_rows "umbrachyx history" "" ""
  token_rows "umbrachyx plain, synced" auth ""
  token_rows "umbrachyx plain, answered first, synced every 10 ms" auth 10
done
