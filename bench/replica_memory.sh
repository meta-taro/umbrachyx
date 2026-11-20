#!/usr/bin/env bash
# Memory held by a replica that keeps the history and by one that keeps
# current values only (`serve --follow --current-values-only`), for the
# same writes to their primary: <sets> SETs of 32-byte values over a space
# of as many keys, then the same number again over the same space. Each
# replica is a restored backup of its primary and follows it over the
# replicas' port, on this machine. The resident size is read once the
# replica holds every record its primary does. The last columns are what a
# replica holds per SET of the second pass, and how long it takes to
# answer again after it is stopped and started, which is the time to read
# its log.
#
# Usage: bench/replica_memory.sh [sets]
set -euo pipefail

sets="${1:-200000}"
primary_port=6391
feed_port=6392
replica_port=6393
branch=bench
dir="$(mktemp -d)"
primary=
replica=
trap '[ -n "$primary" ] && kill "$primary" 2>/dev/null; [ -n "$replica" ] && kill "$replica" 2>/dev/null; rm -rf "$dir"' EXIT

# Resident size in KiB, as ps reports it on Linux and macOS.
rss_kib() {
  ps -o rss= -p "$1" | tr -d ' '
}

# One number out of what a server says of its replication.
said() {
  valkey-cli -p "$1" UMBRA.REPLICATION | sed -n "s/.*\"$2\":\([0-9]*\).*/\1/p"
}

# Waits until the replica holds as many records as the primary. A replica
# that stopped following says why, and never will.
caught_up() {
  local wanted
  wanted="$(said "$primary_port" records)"
  until [ "$(said "$replica_port" records)" = "$wanted" ]; do
    if ! valkey-cli -p "$replica_port" UMBRA.REPLICATION | grep -q '"stopped":null'; then
      valkey-cli -p "$replica_port" UMBRA.REPLICATION >&2
      exit 1
    fi
    sleep 0.2
  done
}

fill() {
  valkey-benchmark -p "$primary_port" -t set -n "$sets" -r "$sets" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
  caught_up
}

now_ms() {
  python3 -c 'import time; print(int(time.time() * 1000))'
}

cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

start_replica() {
  # shellcheck disable=SC2086
  UMBRACHYX_DB="$1" "$bin" serve "$branch" --port "$replica_port" \
    --follow "127.0.0.1:$feed_port" $2 2>>"$dir/replica.serve.log" &
  replica=$!
  until valkey-cli -p "$replica_port" ping >/dev/null 2>&1; do sleep 0.01; done
}

# One primary and one replica of it, started with <options>.
measure() {
  local name="$1" options="$2" run="$dir/$1" before first second stopped again
  mkdir "$run"
  UMBRACHYX_DB="$run/primary.log" "$bin" branch "$branch" --from main >/dev/null
  # The keys written below share one subject. Written once before the
  # backup, so the replica has that subject's encryption key: nothing
  # between these two servers is encrypted, and no key is sent.
  UMBRACHYX_DB="$run/primary.log" "$bin" commit "$branch" key:seed=x \
    --who bench --why "bench/replica_memory.sh" >/dev/null
  UMBRACHYX_DB="$run/primary.log" "$bin" backup "$run/backup" >/dev/null
  UMBRACHYX_DB="$run/replica.log" "$bin" restore "$run/backup" >/dev/null
  UMBRACHYX_DB="$run/primary.log" "$bin" serve "$branch" --port "$primary_port" \
    --feed-replicas-on "$feed_port" 2>"$dir/$name.primary.serve.log" &
  primary=$!
  until valkey-cli -p "$primary_port" ping >/dev/null 2>&1; do sleep 0.1; done
  start_replica "$run/replica.log" "$options"
  before="$(rss_kib "$replica")"
  fill
  first="$(rss_kib "$replica")"
  fill
  second="$(rss_kib "$replica")"

  kill "$replica"
  wait "$replica" 2>/dev/null || true
  stopped="$(now_ms)"
  start_replica "$run/replica.log" "$options"
  again="$(($(now_ms) - stopped))"
  caught_up
  echo "\"$name\",\"$sets\",\"$before\",\"$first\",\"$second\",\"$(((second - first) * 1024 / sets))\",\"$again\",\"$(rss_kib "$replica")\""

  kill "$replica" "$primary"
  wait "$replica" "$primary" 2>/dev/null || true
  replica=
  primary=
}

echo '"replica","sets","rss_kib_empty","rss_kib","rss_kib_twice","second_pass_bytes_per_set","restart_ms","rss_kib_restarted"'
measure "keeps the history" ""
measure "keeps current values only" "--current-values-only"
