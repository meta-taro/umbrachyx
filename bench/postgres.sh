#!/usr/bin/env bash
# PostgreSQL's side of docs/comparison.md: reading and updating one row by
# its primary key, the nearest thing it has to GET and SET, over loopback
# TCP, with 32-byte values in a table of 1,000,000 rows. One client, then
# 50 clients reading, 8, 64 and 256 clients updating, and one client
# reading while 50 others keep updating.
#
# Every update is its own transaction, synced before it is acknowledged
# (fsync and synchronous_commit on, as PostgreSQL ships). Statements are
# prepared once per connection, so no row measures parsing SQL. The server
# is a fresh one made here with the installed PostgreSQL, its settings as
# shipped except shared_buffers (1 GB, so the table is held in memory, as
# the other servers hold theirs) and max_connections.
#
# pgbench reports the rate; the latencies are read from its per-request
# log. PostgreSQL keeps one current value per row here: it is the baseline
# of a database without history, not a like-for-like of what Umbrachyx
# keeps.
#
# Usage: bench/postgres.sh [requests] [concurrent-requests]
set -euo pipefail

requests="${1:-100000}"
concurrent_requests="${2:-200000}"
port=6432
keys=1000000
# A 32-byte value, as the other scripts write.
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
dir="$(mktemp -d)"
writers=

bin="$(pg_config --bindir 2>/dev/null || true)"
if [ ! -x "$bin/initdb" ]; then
  bin="$(ls -d /usr/lib/postgresql/*/bin | sort -V | tail -1)"
fi
export PATH="$bin:$PATH" PGHOST=127.0.0.1 PGPORT="$port" PGUSER=bench PGDATABASE=postgres

trap '[ -n "$writers" ] && kill "$writers" 2>/dev/null; pg_ctl -D "$dir/data" -m immediate stop >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

initdb -D "$dir/data" -U bench --auth=trust >/dev/null
pg_ctl -D "$dir/data" -l "$dir/postgres.log" -w start \
  -o "-p $port -c listen_addresses=127.0.0.1 -c unix_socket_directories=$dir -c shared_buffers=1GB -c max_connections=400" \
  >/dev/null

psql -q -v ON_ERROR_STOP=1 <<SQL
CREATE TABLE kv (key text PRIMARY KEY, value text NOT NULL);
INSERT INTO kv SELECT 'key:' || lpad(n::text, 12, '0'), '$value' FROM generate_series(0, $keys - 1) AS n;
VACUUM ANALYZE kv;
CHECKPOINT;
SQL

cat >"$dir/get.sql" <<SQL
\set n random(0, $keys - 1)
SELECT value FROM kv WHERE key = 'key:' || lpad(:n::text, 12, '0');
SQL
cat >"$dir/set.sql" <<SQL
\set n random(0, $keys - 1)
UPDATE kv SET value = '$value' WHERE key = 'key:' || lpad(:n::text, 12, '0');
SQL

# One pgbench run of <total> requests from <clients> connections, as a CSV
# row: the rate pgbench reports, and the median and 99th percentile of the
# latencies in its log, which are in microseconds.
bench() {
  local label="$1" test="$2" script="$3" clients="$4" total="$5" log rate
  log="$dir/log.$RANDOM"
  rate="$(pgbench -n -M prepared -f "$dir/$script.sql" -c "$clients" -j "$(nproc)" \
    -t $((total / clients)) -l --log-prefix "$log" 2>/dev/null |
    sed -n 's/^tps = \([0-9.]*\) (without initial connection time).*/\1/p')"
  cat "$log".* | awk '{ print $3 }' | sort -n |
    awk -v label="$label" -v test="$test" -v rate="$rate" '
      { latency[NR] = $1 }
      END {
        p50 = latency[int((NR + 1) / 2)]
        p99 = latency[int(NR * 0.99) > 0 ? int(NR * 0.99) : 1]
        printf "\"%s\",\"%s\",\"%s\",\"%.3f\",\"%.3f\"\n", label, test, rate, p50 / 1000, p99 / 1000
      }'
  rm -f "$log".*
}

echo '"persistence","test","rps","p50_latency_ms","p99_latency_ms"'
bench "every commit synced" GET get 1 "$requests"
bench "every commit synced" UPDATE set 1 "$requests"
bench "every commit synced, 50 clients" GET get 50 "$requests"
# More updates than can finish before the reads do; stopped once they have.
pgbench -n -M prepared -f "$dir/set.sql" -c 50 -j "$(nproc)" -t 100000000 >/dev/null 2>&1 &
writers=$!
sleep 1
bench "every commit synced, GET while 50 clients UPDATE" GET get 1 "$requests"
kill "$writers"
wait "$writers" 2>/dev/null || true
writers=
for clients in 8 64 256; do
  bench "every commit synced, $clients clients" UPDATE set "$clients" "$concurrent_requests"
done
