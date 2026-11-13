#!/usr/bin/env bash
# Checks an image the way it is meant to be used: serve on a volume, write
# over the Redis protocol, kill the container, start another on the same
# volume and read the write back; then `docker stop` has to end it at once.
#
# Usage: docker/smoke.sh <image>
set -euo pipefail

image="${1:?usage: docker/smoke.sh <image>}"
name="umbrachyx-smoke-$$"
volume="umbrachyx-smoke-$$"
port=16380
# `docker stop` waits 10 seconds before it kills; ending well inside that
# shows the server stopped on the signal.
stop_limit_seconds=5

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$volume" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Sends one command as the Redis protocol frames it and prints the first
# line of the reply and, for a value, the line after it.
send() {
  local reply
  exec 3<>"/dev/tcp/127.0.0.1/$port"
  printf '*%d\r\n' "$#" >&3
  for word in "$@"; do printf '$%d\r\n%s\r\n' "${#word}" "$word" >&3; done
  IFS= read -r -t 5 reply <&3
  reply="${reply%$'\r'}"
  if [[ "$reply" == \$* && "$reply" != '$-1' ]]; then
    IFS= read -r -t 5 reply <&3
    reply="${reply%$'\r'}"
  fi
  exec 3<&- 3>&-
  printf '%s\n' "$reply"
}

start() {
  # Published to this machine only, as the README says to.
  docker run -d --name "$name" -p "127.0.0.1:$port:6380" -v "$volume:/data" "$image" >/dev/null
  for _ in $(seq 100); do
    if [ "$(send PING 2>/dev/null)" = "+PONG" ]; then return; fi
    sleep 0.1
  done
  docker logs "$name" >&2
  echo "the server did not answer on $port" >&2
  exit 1
}

expect() {
  local what="$1" wanted="$2" got="$3"
  if [ "$got" != "$wanted" ]; then
    echo "FAILED: $what: wanted '$wanted', got '$got'" >&2
    exit 1
  fi
  echo "ok: $what"
}

docker volume create "$volume" >/dev/null
start
expect "runs as an ordinary user" "10001" "$(docker exec "$name" id -u)"
expect "a write is acknowledged" "+OK" "$(send SET smoke/1/value kept)"
docker kill "$name" >/dev/null
docker rm "$name" >/dev/null

start
expect "the write is there after a kill" "kept" "$(send GET smoke/1/value)"
started=$SECONDS
docker stop "$name" >/dev/null
took=$((SECONDS - started))
if [ "$took" -ge "$stop_limit_seconds" ]; then
  echo "FAILED: docker stop took $took s" >&2
  exit 1
fi
echo "ok: docker stop ends the server at once ($took s)"
