#!/usr/bin/env bash
# Sets a fresh machine up for bench/replicated.sh and starts its role.
set -euxo pipefail
sudo shutdown --show
S=/mnt/nvme
for d in /dev/nvme?n1; do lsblk -n "$d" | grep -q part || D="$d"; done
sudo mkfs.ext4 -q "$D" && sudo mkdir -p $S && sudo mount "$D" $S && sudo chown ubuntu:ubuntu $S
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential git pkg-config curl postgresql >/dev/null
sudo systemctl disable --now postgresql >/dev/null 2>&1 || true
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none >/dev/null
export PATH="$HOME/.cargo/bin:$PATH"
git clone --quiet --depth 1 --branch 9.1.3 https://github.com/valkey-io/valkey.git $S/valkey
make -C $S/valkey -j"$(nproc)" BUILD_TLS=no >/dev/null 2>&1
export PATH="$S/valkey/src:$PATH"
git clone --quiet https://github.com/meta-taro/umbrachyx.git $S/umbrachyx
cd $S/umbrachyx && git checkout --quiet eea53bd
cargo build --quiet --release -p umbrachyx --bin umbrachyx --example replicated_load
mkdir -p $S/r
ulimit -n 65536
date -u +%T
echo SETUP-DONE
[ "$#" -eq 0 ] || bench/replicated.sh "$@"
echo ROLE-RETURNED
