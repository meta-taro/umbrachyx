#!/usr/bin/env bash
set -euxo pipefail
S=/mnt/nvme
out=$HOME/umbrachyx-results/2026-11-26-aws-m6id.xlarge-speed
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential git pkg-config curl >/dev/null
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
export PATH="$HOME/.cargo/bin:$PATH"
git clone --quiet --depth 1 --branch 9.1.3 https://github.com/valkey-io/valkey.git $S/valkey
make -C $S/valkey -j4 BUILD_TLS=no >/dev/null
export PATH="$S/valkey/src:$PATH"
for side in old:a9ed3fc new:eea53bd; do
  git clone --quiet https://github.com/meta-taro/umbrachyx.git $S/${side%%:*}
  git -C $S/${side%%:*} checkout --quiet ${side##*:}
  (cd $S/${side%%:*} && cargo build --quiet --release -p umbrachyx)
done
export TMPDIR=$S/tmp
mkdir -p $TMPDIR $out
ulimit -n 65536
cd $S/new
{
  echo "old: $(git -C $S/old rev-parse HEAD)"
  echo "new: $(git rev-parse HEAD)"
  date -u +%FT%TZ
  df -h $S | tail -1
  grep PRETTY_NAME /etc/os-release
  uname -r
  lscpu | grep -E '^(Model name|CPU\(s\)|Thread|Core)'
  free -h | head -2
  cat /sys/kernel/mm/transparent_hugepage/enabled
  rustup show active-toolchain
  valkey-server --version
} > $out/machine.txt
date -u +%T
bench/ab.sh $S/old $S/new 3 > $out/ab.csv
date -u +%T
for run in 1 2 3; do
  mkdir -p $out/run-$run
  bench/umbrachyx.sh > $out/run-$run/umbrachyx.csv
  bench/valkey.sh > $out/run-$run/valkey.csv
  bench/mixed.sh > $out/run-$run/mixed.csv
  date -u +%T
done
echo ALL-DONE
