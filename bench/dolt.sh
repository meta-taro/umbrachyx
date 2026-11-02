#!/usr/bin/env bash
# Dolt's side of docs/comparison.md: the time to diff two branches that
# differ by ten records, over the records written by
#   cargo run --release -p umbrachyx --example compare -- <records> <dir>
#
# Starting `dolt sql` takes about 100 ms, more than the diff itself, so the
# diff is run `repeats` times in one process and the same number of
# `SELECT 1` in another; the difference divided by `repeats` is the time of
# one diff. Each is the median of `runs` such measurements.
#
# Usage: bench/dolt.sh <dir>
set -euo pipefail

input="$(cd "$1" && pwd)"
runs=5
repeats=200
repo="$(mktemp -d)"
trap 'rm -rf "$repo"' EXIT
cd "$repo"

now_ns() { python3 -c 'import time; print(time.time_ns())'; }

# Median wall time in microseconds of one process running `query`
# `repeats` times.
median_us() {
  local query="$1" times=() script
  script="$(mktemp)"
  for _ in $(seq "$repeats"); do echo "$query;"; done > "$script"
  for _ in $(seq "$runs"); do
    local start end
    start="$(now_ns)"
    dolt sql -r csv < "$script" >/dev/null
    end="$(now_ns)"
    times+=("$(( (end - start) / 1000 ))")
  done
  rm -f "$script"
  printf '%s\n' "${times[@]}" | sort -n | sed -n "$(( (runs + 1) / 2 ))p"
}

dolt init --name bench --email bench@example.invalid >/dev/null
dolt sql -q "CREATE TABLE inventory (k VARCHAR(64) PRIMARY KEY, v VARCHAR(64))"
load_start="$(now_ns)"
dolt table import -u inventory "$input/inventory.csv" >/dev/null
load_end="$(now_ns)"
dolt add -A && dolt commit -m load >/dev/null
dolt checkout -b test >/dev/null
dolt sql < "$input/changes.sql" >/dev/null
dolt add -A && dolt commit -m "ten changes" >/dev/null
dolt checkout main >/dev/null

rows="$(dolt sql -r csv -q "SELECT COUNT(*) FROM inventory" | tail -1)"
changed="$(dolt sql -r csv -q "SELECT COUNT(*) FROM dolt_diff('main', 'test', 'inventory')" | tail -1)"
baseline="$(median_us 'SELECT 1')"
diffs="$(median_us "SELECT COUNT(*) FROM dolt_diff('main', 'test', 'inventory')")"
echo "records,changed,load_s,process_with_${repeats}_select_1_ms,one_diff_us"
echo "$rows,$changed,$(( (load_end - load_start) / 1000000000 )),$(( baseline / 1000 )),$(( (diffs - baseline) / repeats ))"
