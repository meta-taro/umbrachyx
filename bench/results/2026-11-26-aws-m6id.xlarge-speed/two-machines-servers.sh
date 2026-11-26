#!/usr/bin/env bash
# Servers for rows driven from another machine: a plain-async table with 2 (the default here), 1 and 4 serving threads, and Valkey everysec.
set -euo pipefail
S=/mnt/nvme; A=SERVER_PRIVATE_ADDRESS; bin=$S/new/target/release/umbrachyx
sudo shutdown --show
ulimit -n 65536
n=0
for args in "" "--threads 1" "--threads 4"; do
  d=$S/net/u$n; mkdir -p $d; export UMBRACHYX_DB=$d/log
  $bin table kv plain-async --who bench --why net >/dev/null
  $bin branch bench --from main >/dev/null
  nohup $bin serve bench --bind $A --port $((6390 + n)) $args >$d/out 2>&1 &
  n=$((n + 1))
done
mkdir -p $S/net/v
nohup $S/valkey/src/valkey-server --bind $A --protected-mode no --port 6399 --dir $S/net/v --save '' --appendonly yes --appendfsync everysec >$S/net/v/out 2>&1 &
sleep 2; ss -ltn | grep -E ':(639[0-9])'
