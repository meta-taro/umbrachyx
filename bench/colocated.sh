#!/usr/bin/env bash
# The server next to noisy neighbours on one machine, as on a small host
# that also runs a web server and mail: other programs take the processors,
# write to the same disk, churn most of the memory and talk over the
# network (stress-ng plays them). It checks three things and measures one:
#
#   1. the server keeps answering under that load, and how much slower;
#   2. killing it mid-write under that load loses no acknowledged write
#      (the server crash test, 100 rounds);
#   3. when it cannot get more memory it stops and says why, and every
#      write it acknowledged before that is there when it starts again.
#
# Usage: bench/colocated.sh <scratch> [seconds]
#   <scratch> is a directory on the disk the log should be on; the
#   neighbours write there too. [seconds] is how long each measured row
#   runs under load (default 20).
set -euo pipefail

scratch="${1:?usage: bench/colocated.sh <scratch> [seconds]}"
seconds="${2:-20}"
port=6390
keys=100000
branch=bench
# The memory the server may have in the third check. Small, so that
# filling it takes seconds.
memory_limit_kib=$((1024 * 1024))
dir="$(mktemp -d -p "$scratch")"
server=
neighbours=
cleanup() {
  [ -n "$neighbours" ] && kill "$neighbours" 2>/dev/null
  [ -n "$server" ] && kill "$server" 2>/dev/null
  rm -rf "$dir"
}
trap cleanup EXIT

command -v stress-ng >/dev/null || { echo "stress-ng is not installed" >&2; exit 1; }
cargo build --quiet --release -p umbrachyx
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/umbrachyx"

start() {
  export UMBRACHYX_DB="$dir/$1.log"
  [ -e "$UMBRACHYX_DB" ] || "$bin" branch "$branch" --from main >/dev/null
  "$bin" serve "$branch" --port "$port" 2>"$dir/$1.stderr" &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do
    kill -0 "$server" 2>/dev/null || { cat "$dir/$1.stderr" >&2; exit 1; }
    sleep 0.1
  done
}

stop() {
  kill "$server" 2>/dev/null || true
  wait "$server" 2>/dev/null || true
  server=
}

# One row: <state> <label> then valkey-benchmark's own options.
row() {
  local state="$1" label="$2"
  shift 2
  valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null | grep -v '"test"' |
    python3 -c 'import csv, sys
for r in csv.reader(sys.stdin):
    print(",".join([sys.argv[1], "\"" + sys.argv[2] + "\"", r[0], r[1], r[4], r[6]]))' "$state" "$label"
}

rows() {
  local state="$1" requests="$2"
  row "$state" "1 client" -t set,get -n "$requests" -c 1
  row "$state" "50 clients" -t set,get -n "$((requests * 10))" -c 50
}

# The neighbours: every processor busy, two writers syncing files on the
# same disk, two programs churning 35% of the memory each, two talking
# over the network.
start_neighbours() {
  stress-ng --cpu "$(nproc)" \
    --hdd 2 --hdd-bytes 256m --hdd-opts fsync --temp-path "$dir" \
    --vm 2 --vm-bytes 35% \
    --sock 2 \
    --timeout 3600 >/dev/null 2>&1 &
  neighbours=$!
  sleep 3
}

stop_neighbours() {
  kill "$neighbours" 2>/dev/null || true
  wait "$neighbours" 2>/dev/null || true
  neighbours=
}

echo "# 1. the server alone, then next to the neighbours"
echo 'state,clients,test,rps,p50_latency_ms,p99_latency_ms'
start rows
valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
# Sized from the quiet run, so a row under load lasts about <seconds>.
quiet_rps="$(row alone "1 client" -t set -n 2000 -c 1 | cut -d, -f4 | cut -d. -f1)"
rows alone "$((quiet_rps * 5))"
start_neighbours
rows crowded "$((quiet_rps * seconds / 4))"
kill -0 "$server" 2>/dev/null || { echo "FAILED: the server stopped under load"; cat "$dir/rows.stderr"; exit 1; }
echo "ok: the server answered throughout"
stop

echo
echo "# 2. killed mid-write next to the neighbours, 100 rounds"
# The verdict is read from what the test printed, so a failing run still
# gets its FAILED line here instead of ending the script at the pipe.
TMPDIR="$dir" cargo test -p umbrachyx --release --test server_crash -- --ignored --nocapture >"$dir/crash.txt" 2>&1 || true
grep -E "^test |test result|panicked|lost" "$dir/crash.txt" || true
grep -q "test result: ok. 1 passed" "$dir/crash.txt" || { echo "FAILED: the crash test did not pass"; exit 1; }
stop_neighbours

echo
echo "# 3. out of memory: the server may have $((memory_limit_kib / 1024)) MiB"
export UMBRACHYX_DB="$dir/memory.log"
"$bin" branch "$branch" --from main >/dev/null
(
  ulimit -v "$memory_limit_kib"
  exec "$bin" serve "$branch" --port "$port" 2>"$dir/memory.stderr"
) &
server=$!
until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
# Writes keys in order, 64 at a time, until the server stops answering;
# prints how many were acknowledged.
acknowledged="$(python3 - "$port" <<'PYTHON'
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])))
f = s.makefile("rb")
value = b"x" * 512
acked = 0
try:
    while True:
        batch = b"".join(
            b"*3\r\n$3\r\nSET\r\n$%d\r\n%s\r\n$%d\r\n%s\r\n" % (len(k), k, len(value), value)
            for k in (b"fill/%d/v" % (acked + i) for i in range(64))
        )
        s.sendall(batch)
        for _ in range(64):
            if f.readline() != b"+OK\r\n":
                raise ConnectionError
            acked += 1
except (ConnectionError, OSError):
    pass
print(acked)
PYTHON
)"
status=0
wait "$server" || status=$?
server=
echo "acknowledged before it stopped: $acknowledged"
echo "exit status: $status"
echo "what it said:"
grep -v "^serving " "$dir/memory.stderr" | sed 's/^/  /' || true
grep -q "memory" "$dir/memory.stderr" || { echo "FAILED: the server stopped without saying it was out of memory"; exit 1; }
[ "$acknowledged" -gt 0 ] || { echo "FAILED: nothing was acknowledged before the limit"; exit 1; }

start memory
missing="$(python3 - "$port" "$acknowledged" <<'PYTHON'
import socket, sys
port, acked = int(sys.argv[1]), int(sys.argv[2])
s = socket.create_connection(("127.0.0.1", port))
f = s.makefile("rb")
missing = 0
for first in range(0, acked, 64):
    ks = [b"fill/%d/v" % i for i in range(first, min(first + 64, acked))]
    s.sendall(b"".join(b"*2\r\n$3\r\nGET\r\n$%d\r\n%s\r\n" % (len(k), k) for k in ks))
    for _ in ks:
        head = f.readline()
        if head == b"$-1\r\n":
            missing += 1
        else:
            f.read(int(head[1:]) + 2)
print(missing)
PYTHON
)"
stop
echo "acknowledged writes missing after it started again: $missing"
[ "$missing" -eq 0 ] || { echo "FAILED: acknowledged writes were lost"; exit 1; }
echo "ok: it said why it stopped, and every acknowledged write is there"
