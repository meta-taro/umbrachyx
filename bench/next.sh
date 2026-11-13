#!/usr/bin/env bash
# One sequential run for the release machine: is the key index faster
# there, where do GETs from 50 clients lose a third, and where does the
# writing thread's time go. Everything goes to <scratch>/out.
#
# Usage: next.sh <scratch> <old-commit> <new-commit>   (DRY=1: tiny counts)
set -euo pipefail
scratch="$1"; old_commit="$2"; new_commit="$3"
out="$scratch/out"; mkdir -p "$out" "$scratch/tmp"
export TMPDIR="$scratch/tmp"
n() { if [ -n "${DRY:-}" ]; then echo $(( $1 / 100 )); else echo "$1"; fi; }
rounds=3; seconds=10
if [ -n "${DRY:-}" ]; then rounds=1; seconds=2; fi
port=6390; keys=$(n 1000000)
step() { echo "== $(date -u +%T) $*" | tee -a "$out/progress.txt"; }

step setup
if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  sudo apt-get update -qq
  sudo apt-get install -y -qq build-essential git pkg-config curl python3 >/dev/null
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
fi
export PATH="$HOME/.cargo/bin:$scratch/valkey/src:$PATH"
if ! command -v valkey-benchmark >/dev/null; then
  git clone --quiet --depth 1 --branch 9.1.3 https://github.com/valkey-io/valkey.git "$scratch/valkey"
  make -C "$scratch/valkey" -j"$(nproc)" BUILD_TLS=no >/dev/null
fi
if ! command -v perf >/dev/null; then
  sudo apt-get install -y -qq linux-tools-common "linux-tools-$(uname -r)" >/dev/null 2>&1 || true
fi

step build
repo="${REPO:-https://github.com/meta-taro/umbrachyx.git}"
for side in old new; do
  commit="$old_commit"; [ "$side" = new ] && commit="$new_commit"
  if [ ! -d "$scratch/$side" ]; then git clone --quiet "$repo" "$scratch/$side"; fi
  (cd "$scratch/$side" && git checkout --quiet "$commit" && cargo build --quiet --release --locked -p umbrachyx)
done

step machine
{
  date -u +%FT%TZ; uname -r
  lscpu | grep -E '^(Model name|CPU\(s\)|Thread|Core)'
  (cd "$scratch/new" && rustup show active-toolchain)
  valkey-benchmark --version
  echo "old $(git -C "$scratch/old" rev-parse HEAD)"
  echo "new $(git -C "$scratch/new" rev-parse HEAD)"
  echo "siblings: $(cat /sys/devices/system/cpu/cpu*/topology/thread_siblings_list 2>/dev/null | tr '\n' ' ')"
  df -h "$scratch" | tail -1
} >"$out/machine.txt"

step "old against new, in turn"
if [ -n "${DRY:-}" ]; then echo "dry run: bench/ab.sh skipped (fixed counts)" >"$out/ab.csv"
else "$scratch/new/bench/ab.sh" "$scratch/old" "$scratch/new" "$rounds" >"$out/ab.csv"; fi

server=
start() { # start <binary> [taskset cpu list]
  dir="$(mktemp -d)"; export UMBRACHYX_DB="$dir/log"
  "$1" branch bench --from main >/dev/null
  if [ -n "${2:-}" ]; then taskset -c "$2" "$1" serve bench --port "$port" 2>/dev/null &
  else "$1" serve bench --port "$port" 2>/dev/null & fi
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
}
stop() { kill "$server"; wait "$server" 2>/dev/null || true; server=; rm -rf "$dir"; }
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT
rate() { # rate [client cpu list or -] <benchmark args>
  local cpus="$1"; shift
  if [ "$cpus" = - ]; then valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null
  else taskset -c "$cpus" valkey-benchmark -p "$port" -r "$keys" -d 32 --csv "$@" 2>/dev/null; fi |
    grep -v '"test"' | cut -d, -f2 | tr -d '"' | tr '\n' ' '
}
bin="$scratch/new/target/release/umbrachyx"

step "where the client and the serving threads run"
# cpu0's sibling shares its core; a processor that is neither is on another core.
sibling="$(cut -d, -f2 /sys/devices/system/cpu/cpu0/topology/thread_siblings_list 2>/dev/null | cut -d- -f2 || echo 0)"
other=1; [ "$sibling" = 1 ] && other=2
last=$(( $(nproc) - 1 )); [ "$other" -gt "$last" ] && other="$last"
other_sibling="$(cut -d, -f2 "/sys/devices/system/cpu/cpu$other/topology/thread_siblings_list" 2>/dev/null | cut -d- -f2 || echo "$other")"
{
  echo "cpu0 shares a core with cpu$sibling; cpu$other with cpu$other_sibling"
  for case in "free:-:-" "server on one core, client on the other:0,$sibling:$other" \
              "server on one core, client on both threads of the other:0,$sibling:$other,$other_sibling" \
              "server on a thread of each core, client beside one:0,$other:$sibling" \
              "server free, client on cpu$other:-:$other" "server and client on one core:0,$sibling:0,$sibling"; do
    IFS=: read -r label server_cpus client_cpus <<<"$case"
    for process in 1 2; do
      if [ "$server_cpus" = - ]; then start "$bin"; else start "$bin" "$server_cpus"; fi
      echo -n "$label, server $process: "
      for _ in 1 2 3 4 5 6; do rate "$client_cpus" -t get -n "$(n 400000)" -c 50; done
      echo
      stop
    done
  done
} >"$out/placement.txt"

step "GETs sent 16 at a time, after more and more writes"
{
  start "$bin"
  for written in 1 2 3 4; do
    echo -n "after about $written million writes: "
    for _ in 1 2 3; do rate - -t get -n "$(n 2000000)" -c 50 -P 16; done
    echo
    valkey-benchmark -p "$port" -t set -n "$keys" -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1
  done
  stop
} >"$out/pipelined-gets.txt"

step "profile"
if command -v perf >/dev/null; then
  start "$bin"
  valkey-benchmark -p "$port" -t set -n 100000000 -r "$keys" -d 32 -c 50 -P 64 -q >/dev/null 2>&1 &
  load=$!
  sleep 3
  perf record -F 999 -g -p "$server" -o "$scratch/tmp/set.data" -- sleep "$seconds" >/dev/null 2>&1 || true
  kill "$load" 2>/dev/null || true; wait "$load" 2>/dev/null || true
  {
    echo "# 50 clients sending writes 64 at a time: time by thread"
    perf report -i "$scratch/tmp/set.data" --no-children -g none --sort comm 2>/dev/null | grep -v '^#' | grep '%' | cut -c1-70 | awk "NR<=8"
    echo "# the thread that writes, by function, own time"
    perf report -i "$scratch/tmp/set.data" --no-children -g none --comm "resp commits" --sort symbol 2>/dev/null | grep -v '^#' | grep '%' | cut -c1-150 | awk "NR<=50"
    echo "# the thread that writes, by function, with what each calls"
    perf report -i "$scratch/tmp/set.data" --children -g none --comm "resp commits" --sort symbol 2>/dev/null | grep -v '^#' | grep '%' | cut -c1-150 | awk "NR<=60"
  } >"$out/set-profile.txt"
  valkey-benchmark -p "$port" -t get -n 100000000 -r "$keys" -d 32 -c 50 -P 16 -q >/dev/null 2>&1 &
  load=$!
  sleep 2
  perf record -F 999 -g -p "$server" -o "$scratch/tmp/get.data" -- sleep "$seconds" >/dev/null 2>&1 || true
  kill "$load" 2>/dev/null || true; wait "$load" 2>/dev/null || true
  {
    echo "# 50 clients sending GETs 16 at a time: time by thread and by where the code is"
    perf report -i "$scratch/tmp/get.data" --no-children -g none --sort comm,dso 2>/dev/null | grep -v '^#' | grep '%' | cut -c1-90 | awk "NR<=12"
    echo "# by function, own time"
    perf report -i "$scratch/tmp/get.data" --no-children -g none --sort symbol 2>/dev/null | grep -v '^#' | grep '%' | cut -c1-150 | awk "NR<=40"
  } >"$out/get-profile.txt"
  stop
else
  echo "perf is not installed" >"$out/set-profile.txt"
fi

(cd "$out" && sha256sum ./*.txt ./*.csv >sums.sha256)
step ALL-DONE
