#!/usr/bin/env bash
# Every benchmark and robustness test on a fresh Ubuntu 24.04 machine, for
# a release. Installs what it needs, builds Valkey from its release tag,
# runs bench/umbrachyx.sh, bench/valkey.sh and bench/connections.sh several
# times, then the robustness tests, and writes everything with a
# description of the machine to ~/umbrachyx-results/<date>-<machine>/.
#
# The logs go to <scratch>, which should be on the machine's local NVMe
# disk rather than a network disk: what a sync costs decides most of the
# numbers.
#
# Usage: bench/cloud.sh <machine-name> <scratch> [runs]
set -euo pipefail

machine="$1"
scratch="$2"
runs="${3:-3}"
valkey_version=9.1.3
repo="$(cd "$(dirname "$0")/.." && pwd)"
out="$HOME/umbrachyx-results/$(date -u +%F)-$machine"

sudo apt-get update -qq
sudo apt-get install -y -qq build-essential git pkg-config curl >/dev/null
if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
fi
export PATH="$HOME/.cargo/bin:$PATH"

valkey="$scratch/valkey"
if [ ! -x "$valkey/src/valkey-server" ]; then
  git clone --quiet --depth 1 --branch "$valkey_version" https://github.com/valkey-io/valkey.git "$valkey"
  make -C "$valkey" -j"$(nproc)" BUILD_TLS=no >/dev/null
fi
export PATH="$valkey/src:$PATH"

# The scripts and tests put their logs in the temporary directory.
export TMPDIR="$scratch/tmp"
mkdir -p "$TMPDIR" "$out"
ulimit -n 65536
cd "$repo"

{
  echo "machine: $machine"
  echo "commit: $(git rev-parse HEAD)"
  echo "date: $(date -u +%FT%TZ)"
  echo "scratch: $(df -h "$scratch" | tail -1)"
  grep PRETTY_NAME /etc/os-release
  uname -r
  lscpu | grep -E '^(Model name|CPU\(s\)|Thread|Core)'
  free -h | head -2
  rustup show active-toolchain
  valkey-server --version
} >"$out/machine.txt"

for run in $(seq 1 "$runs"); do
  mkdir -p "$out/run-$run"
  bench/umbrachyx.sh >"$out/run-$run/umbrachyx.csv"
  bench/valkey.sh >"$out/run-$run/valkey.csv"
  bench/connections.sh >"$out/run-$run/connections.csv" 2>"$out/run-$run/connections.stderr.txt"
done

{
  cargo test -q -p umbrachyx --release --test server_crash -- --ignored --nocapture
  cargo test -q -p umbrachyx --test crash -- --ignored --nocapture
  cargo test -q -p umbrachyx --lib --release ten_thousand_seeds -- --ignored --nocapture
} >"$out/robustness.txt" 2>&1

echo "$out"
