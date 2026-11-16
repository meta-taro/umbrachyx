#!/usr/bin/env bash
# SPIKE: where a write's time goes when every variant of the plain-table
# spike stops near 110,000 writes a second. For each variant it starts a
# server, fills it, puts it under load and samples it with perf: the
# share of each thread, and inside the writing thread and the serving
# threads, the functions by their own time and with what they call.
#
# Usage: bench/spike-profile.sh <scratch>
set -euo pipefail

scratch="$1"
valkey_version=9.1.3
keys="${KEYS:-1000000}"
seconds="${SECONDS_SAMPLED:-12}"
port=6390
branch=bench
value=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
export TMPDIR="$scratch"
dir="$(mktemp -d)"
server=
load=
trap '[ -n "$load" ] && kill "$load" 2>/dev/null; [ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

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
if ! command -v perf >/dev/null; then
  sudo apt-get install -y -qq linux-tools-common "linux-tools-$(uname -r)" >/dev/null 2>&1 || true
fi
# An ordinary user may not profile unless the kernel is told so.
sudo sysctl -w kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0 >/dev/null 2>&1 || true

# The release build with names and frame pointers, so perf can say which
# function called which. Its own target directory: other builds stay as
# they are.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$scratch/target-profile}"
CARGO_PROFILE_RELEASE_DEBUG=line-tables-only RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --quiet --release -p umbrachyx
bin="$CARGO_TARGET_DIR/release/umbrachyx"

report() {
  local data="$1" title="$2" comm="$3" own="$4" children="$5"
  echo "## $title, own time"
  perf report -i "$data" --no-children -g none --comm "$comm" --sort symbol 2>/dev/null |
    grep -v '^#' | grep '%' | cut -c1-120 | awk "NR<=$own"
  echo "## $title, with what each calls"
  perf report -i "$data" --children -g none --comm "$comm" --sort symbol 2>/dev/null |
    grep -v '^#' | grep '%' | cut -c1-120 | awk "NR<=$children"
}

# <title> <table or ""> <nowait ms or ""> <token: yes or ""> then the load's options
profile() {
  local title="$1" table="$2" nowait="$3" token="$4" prefix auth=() serve=()
  shift 4
  export UMBRACHYX_DB="$dir/$RANDOM.log"
  if [ -n "$token" ]; then
    rm -f "$dir/root.key"
    serve=(--public-key "$("$bin" keygen "$dir/root.key")")
    "$bin" token issue --key "$dir/root.key" --subject agent:bench --branch "$branch" \
      --prefix "${table:-hist}/" --columns "key" --operations read,write --purpose bench --expires-in 8h \
      >"$dir/token"
    auth=(-a "$(cat "$dir/token")")
  fi
  "$bin" branch "$branch" --from main >/dev/null
  # shellcheck disable=SC2086
  env ${table:+UMBRACHYX_SPIKE_PLAIN=$table} ${nowait:+UMBRACHYX_SPIKE_NOWAIT=$nowait} \
    "$bin" serve "$branch" --port "$port" ${serve[@]+"${serve[@]}"} 2>/dev/null &
  server=$!
  until valkey-cli -p "$port" ping >/dev/null 2>&1; do sleep 0.1; done
  # With a token the key's last part is the column the token names.
  prefix="${table:-hist}/"
  local key="${prefix}bench/key:__rand_int__"
  local space="$keys"
  # As in bench/umbrachyx.sh: each of these keys is a subject of its own.
  if [ -n "$token" ]; then
    key="${prefix}__rand_int__/key"
    space=$((keys / 10))
  fi
  valkey-benchmark -p "$port" -n "$((space * 2))" -r "$space" -c 50 -P 64 -q ${auth[@]+"${auth[@]}"} \
    SET "$key" "$value" >/dev/null 2>&1
  echo
  echo "# $title"
  echo -n "writes a second: "
  valkey-benchmark -p "$port" -r "$space" --csv -n "$((keys * 2))" "$@" ${auth[@]+"${auth[@]}"} \
    SET "$key" "$value" 2>/dev/null | grep -v '"test"' | cut -d, -f2
  valkey-benchmark -p "$port" -r "$space" -n 100000000 "$@" ${auth[@]+"${auth[@]}"} -q \
    SET "$key" "$value" >/dev/null 2>&1 &
  load=$!
  sleep 3
  echo "## processor use by thread, the server's then the client's (one 3 s sample)"
  top -H -b -d 3 -n 2 -p "$server" -p "$load" | awk '/^top -/{n++} n==2' |
    awk 'NR>7 && $9+0>1.0 {print $9"%", $12, $13, $14}' | head -12
  if command -v perf >/dev/null; then
    perf record -F 999 -g -p "$server" -o "$dir/perf.data" -- sleep "$seconds" >/dev/null 2>&1 || true
    echo "## share of the samples by thread"
    perf report -i "$dir/perf.data" --no-children -g none --sort comm 2>/dev/null |
      grep -v '^#' | grep '%' | cut -c1-70 | awk "NR<=8"
    report "$dir/perf.data" "the thread that writes" "resp commits" 28 34
    report "$dir/perf.data" "the threads that serve" "resp loop 0,resp loop 1" 28 30
  else
    echo "perf is not installed"
  fi
  kill "$load" 2>/dev/null || true
  wait "$load" 2>/dev/null || true
  load=
  kill "$server"
  wait "$server" 2>/dev/null || true
  server=
  rm -f "$UMBRACHYX_DB" "$UMBRACHYX_DB.keys" "$dir/perf.data"
}

echo "commit: $(git rev-parse HEAD)"
echo "$(nproc) processors, $(uname -r)"
profile "plain, answered first, 50 clients sending 64 at a time" kv 10 "" -c 50 -P 64
profile "plain, answered first, 64 clients one at a time" kv 10 "" -c 64
profile "plain, synced, 50 clients sending 64 at a time" kv "" "" -c 50 -P 64
profile "history, 50 clients sending 64 at a time" "" "" "" -c 50 -P 64
profile "plain, answered first, with a token, 50 clients sending 64 at a time" kv 10 yes -c 50 -P 64
