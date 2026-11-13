#!/bin/sh
# What the container runs. `serve` (the default) serves the branch named by
# UMBRACHYX_BRANCH, made from main on the first start, since clients write
# to a branch and never to main. Options after `serve` are passed on, such
# as --public-key and --require-auth. Anything else is run as an
# `umbrachyx` command against the same log.
set -eu

if [ "${1:-serve}" != serve ]; then
  exec umbrachyx "$@"
fi
[ "$#" -gt 0 ] && shift

branch="${UMBRACHYX_BRANCH:-app}"
if ! umbrachyx log "$branch" --limit 1 >/dev/null 2>&1; then
  umbrachyx branch "$branch" --from main
fi
# Inside a container 127.0.0.1 is the container's own, which the host
# cannot reach, so the server listens on all of the container's addresses.
# Who reaches it is decided by how the port is published.
exec umbrachyx serve "$branch" --bind 0.0.0.0 "$@"
