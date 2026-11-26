#!/usr/bin/env bash
set -euxo pipefail
export PATH="/mnt/nvme/valkey/src:$HOME/.cargo/bin:$PATH"
cd /mnt/nvme/umbrachyx
ulimit -n 65536
P=PRIMARY
date -u +%T
bench/replicated.sh client $P REPLICA_1
date -u +%T
bench/replicated.sh client $P REPLICA_1 REPLICA_2
date -u +%T
bench/replicated.sh client $P REPLICA_1 REPLICA_2 REPLICA_3 REPLICA_4
date -u +%T
echo CLIENT-DONE
