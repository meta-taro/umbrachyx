#!/usr/bin/env bash
# One primary, its replicas and a client, each on a machine of its own:
# what a replica adds and what it costs, for Umbrachyx and for Valkey set
# up the same way. Every machine runs this script once with its role; the
# server machines are then left alone and the client drives every row
# over the database ports.
#
#   bench/replicated.sh primary <own address> <dir>
#   bench/replicated.sh replica <own address> <primary address> <dir>
#   bench/replicated.sh client  <primary address> <replica address>...
#
# <dir> is on the local disk and holds the logs. The primary writes
# <dir>/seed, a backup of each of its stores made before anything else
# is written; a replica waits until its own <dir>/seed is there, restores
# it and follows. Copying that directory from the primary's machine to
# each replica's is the one step a person does: it holds encryption keys,
# and nothing between these servers is encrypted.
#
# The pairs, each a primary and a replica of it on every replica machine:
#
#   u-history   Umbrachyx; the replica keeps the history
#   u-waits     the same, the primary answering a write only once a
#               replica holds it (--wait-for-replica)
#   u-current   Umbrachyx; the replica keeps current values only
#   valkey      Valkey with appendfsync always on both; rows marked
#               WAITAOF send `WAITAOF 1 1 0` after every SET, which waits
#               for the primary's disk and one replica's
#
# A row is to be read beside the row that promises the same when a write
# is answered: u-history beside valkey (the primary's disk), u-waits
# beside valkey with WAITAOF (the primary's disk and a replica's).
#
# Servers listen on <own address> only. Away from this machine's own
# loopback that must be a private address, and the machine must have a
# shutdown pending, or the script refuses to start: a measuring machine
# left running costs more than every run of this script together.
#
# To rehearse on one machine, give 127.0.0.1 for every address and set
# UMBRACHYX_BENCH_REPLICA_OFFSET=1000 for the replica and the client: the
# replica's servers then listen that many ports higher.
#
# Usage: see above. UMBRACHYX_BENCH_SECONDS (default 10) is how long each
# row of load runs, UMBRACHYX_BENCH_KEYS (default 1000000) the keys filled.
set -euo pipefail

role="${1:-}"
seconds="${UMBRACHYX_BENCH_SECONDS:-10}"
keys="${UMBRACHYX_BENCH_KEYS:-1000000}"
offset="${UMBRACHYX_BENCH_REPLICA_OFFSET:-0}"
branch=bench
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
# The port clients use for each pair; an Umbrachyx primary hands its log
# to replicas one port higher.
u_pairs="u-history:6390 u-waits:6490 u-current:6590"
valkey_port=6399

usage() {
  sed -n '2,12p' "$0" >&2
  exit 2
}

# Refuses an address servers must not listen on, and a machine that
# would not turn itself off.
check_machine() {
  local address="$1"
  case "$address" in
    127.*) return 0 ;;
    10.* | 192.168.* | 172.1[6-9].* | 172.2[0-9].* | 172.3[01].*) ;;
    *)
      echo "replicated.sh: $address is not a private address: servers here ask for no token" >&2
      exit 1
      ;;
  esac
  if ! shutdown --show >/dev/null 2>&1; then
    echo "replicated.sh: no shutdown is pending on this machine; start it with one (see .claude/rules/benchmarks.md)" >&2
    exit 1
  fi
}

build() {
  cargo build --quiet --release -p umbrachyx --bin umbrachyx --example replicated_load
  target="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release"
  bin="$target/umbrachyx"
  load="$target/examples/replicated_load"
}

wait_for_port() {
  until valkey-cli -h "$1" -p "$2" ping >/dev/null 2>&1; do sleep 0.2; done
}

primary() {
  local address="$1" dir="$2" pair name port options
  check_machine "$address"
  build
  mkdir -p "$dir/seed"
  for pair in $u_pairs; do
    name="${pair%%:*}"
    port="${pair##*:}"
    export UMBRACHYX_DB="$dir/$name.log"
    "$bin" branch "$branch" --from main >/dev/null
    # The keys of every row share one subject. Written once before the
    # backup, so a replica has that subject's encryption key.
    "$bin" commit "$branch" key:seed=x --who bench --why bench/replicated.sh >/dev/null
    "$bin" backup "$dir/seed/$name" >/dev/null
    options=(--feed-replicas-on "$((port + 1))")
    if [ "$name" = u-waits ]; then
      options+=(--wait-for-replica 60)
    fi
    "$bin" serve "$branch" --bind "$address" --port "$port" "${options[@]}" \
      2>"$dir/$name.serve.log" &
    wait_for_port "$address" "$port"
  done
  unset UMBRACHYX_DB
  mkdir -p "$dir/valkey"
  valkey-server --bind "$address" --protected-mode no --port "$valkey_port" \
    --dir "$dir/valkey" --save '' --appendonly yes --appendfsync always \
    --daemonize yes --pidfile "$dir/valkey.pid" --logfile "$dir/valkey.log"
  wait_for_port "$address" "$valkey_port"
  touch "$dir/seed/complete"
  echo "primary: serving on $address; copy $dir/seed to every replica's <dir>/seed"
  wait
}

replica() {
  local address="$1" primary_address="$2" dir="$3" pair name port options
  check_machine "$address"
  build
  mkdir -p "$dir"
  echo "replica: waiting for $dir/seed"
  until [ -e "$dir/seed/complete" ]; do sleep 1; done
  for pair in $u_pairs; do
    name="${pair%%:*}"
    port="${pair##*:}"
    export UMBRACHYX_DB="$dir/$name.replica.log"
    "$bin" restore "$dir/seed/$name" >/dev/null
    options=(--follow "$primary_address:$((port + 1))")
    if [ "$name" = u-current ]; then
      options+=(--current-values-only)
    fi
    "$bin" serve "$branch" --bind "$address" --port "$((port + offset))" "${options[@]}" \
      2>"$dir/$name.replica.serve.log" &
    wait_for_port "$address" "$((port + offset))"
  done
  unset UMBRACHYX_DB
  mkdir -p "$dir/valkey-replica"
  valkey-server --bind "$address" --protected-mode no --port "$((valkey_port + offset))" \
    --dir "$dir/valkey-replica" --save '' --appendonly yes --appendfsync always \
    --replicaof "$primary_address" "$valkey_port" \
    --daemonize yes --pidfile "$dir/valkey-replica.pid" --logfile "$dir/valkey-replica.log"
  wait_for_port "$address" "$((valkey_port + offset))"
  echo "replica: following $primary_address from $address"
  wait
}

# The rows of one pair: <label> <port> <wait> <primary> <replica>...
rows() {
  local label="$1" port="$2" wait="$3" primary_address="$4" readers replicas=() address
  shift 4
  for address in "$@"; do
    replicas+=("$address:$((port + offset))")
  done
  readers=$((50 * ${#replicas[@]}))
  # Every key written once, and every replica caught up: a write is
  # asked of each until it is there.
  valkey-benchmark -h "$primary_address" -p "$port" -n "$keys" -r "$keys" -c 50 -P 64 -q \
    SET "key:__rand_int__" "$value" >/dev/null 2>&1
  for address in "${replicas[@]}"; do
    "$load" lag warm 1 "$primary_address:$port" none "$address" >/dev/null
  done
  "$load" lag "$label" 1000 "$primary_address:$port" "$wait" "${replicas[0]}" \
    | sed 's/^/"lag",/' >>"$out/lag.csv"
  "$load" load "$label, reads only" "$seconds" "$primary_address:$port" 0 "$wait" "$readers" \
    "${replicas[@]}" >>"$out/load.csv"
  "$load" load "$label, reads beside 8 writers" "$seconds" "$primary_address:$port" 8 "$wait" \
    "$readers" "${replicas[@]}" >>"$out/load.csv"
  for writers in 8 64 256; do
    "$load" load "$label, writes only" "$seconds" "$primary_address:$port" "$writers" "$wait" 0 \
      >>"$out/load.csv"
  done
}

client() {
  local primary_address="$1" pair address
  shift
  [ "$#" -ge 1 ] || usage
  build
  out="${UMBRACHYX_BENCH_OUT:-$HOME/umbrachyx-results/$(date +%F)-replicated-$#}"
  mkdir -p "$out"
  for pair in $u_pairs "valkey:$valkey_port"; do
    wait_for_port "$primary_address" "${pair##*:}"
    for address in "$@"; do
      wait_for_port "$address" "$((${pair##*:} + offset))"
    done
  done
  {
    echo "date: $(date -u +%FT%TZ)"
    echo "commit: $(git rev-parse HEAD)"
    echo "primary: $primary_address"
    echo "replicas: $*"
    echo "keys: $keys, seconds a row: $seconds"
    valkey-server --version
  } >"$out/run.txt"
  echo '"what","label","writes","ms_p50","ms_p99","ms_max"' >"$out/lag.csv"
  echo '"label","writers","readers","replicas","writes_per_second","write_ms_p50","write_ms_p99","reads_per_second","read_ms_p50","read_ms_p99"' >"$out/load.csv"
  for pair in $u_pairs; do
    rows "${pair%%:*}" "${pair##*:}" none "$primary_address" "$@"
  done
  rows "valkey" "$valkey_port" none "$primary_address" "$@"
  rows "valkey, WAITAOF 1 1" "$valkey_port" waitaof "$primary_address" "$@"
  echo "client: rows are in $out"
}

case "$role" in
  primary) [ "$#" -eq 3 ] || usage; primary "$2" "$3" ;;
  replica) [ "$#" -eq 4 ] || usage; replica "$2" "$3" "$4" ;;
  client) [ "$#" -ge 3 ] || usage; shift; client "$@" ;;
  *) usage ;;
esac
