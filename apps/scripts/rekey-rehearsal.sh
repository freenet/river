#!/usr/bin/env bash
# Rehearsal of a re-key for the native app (freenet/river#889), on one isolated Freenet node.
#
# An OLDER River app (built from <old commit>, whose chat delegate and room contract are earlier
# generations than this checkout's) seeds the node: it creates three rooms, sends a message in one
# and leaves another. Then this checkout's app runs on the same node and must show the two rooms
# kept (with the message, which lives under the older room contract), not the one left, and send
# under the current room contract. Then the older app writes a new room (a late write from an
# earlier generation) and this checkout's app, started again, must show it too.
# apps/river-app/examples/rekey_rehearsal.rs is the driver; it is copied into the older checkout.
#
# Usage: apps/scripts/rekey-rehearsal.sh <work dir> <old commit>
#   FREENET_BIN          the freenet binary to run (default: freenet on PATH)
#   REHEARSAL_PORT_BASE  first of the two ports used (default 39500): network, API
#   CARGO                the cargo command (default: cargo), e.g. a wrapper that takes a lock
#
# The node runs on loopback with no peers, its own empty directories and telemetry off. Nothing
# outside <work dir> is written, apart from cargo's build output for this checkout.
set -euo pipefail

WORK="${1:?usage: $0 <work dir> <old commit>}"
OLD="${2:?usage: $0 <work dir> <old commit>}"
FREENET_BIN="${FREENET_BIN:-freenet}"
BASE="${REHEARSAL_PORT_BASE:-39500}"
NET=$BASE API=$((BASE + 1))
APPS="$(cd "$(dirname "$0")/.." && pwd)"
REPO="$(cd "$APPS/.." && pwd)"

mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
for d in node old app; do
  if [ -e "$WORK/$d" ]; then
    echo "$WORK/$d exists; use an empty work dir" >&2
    exit 1
  fi
done
mkdir -p "$WORK/node/cfg" "$WORK/node/data" "$WORK/node/log" "$WORK/app"
echo "freenet: $("$FREENET_BIN" --version | head -n1)"

openssl genpkey -algorithm X25519 -out "$WORK/node/x25519.pem" 2>/dev/null
openssl pkey -in "$WORK/node/x25519.pem" -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n' \
  > "$WORK/node/transport_keypair"

NODE_PID=""
cleanup() {
  status=$?
  if [ -n "$NODE_PID" ]; then
    kill "$NODE_PID" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$NODE_PID" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$NODE_PID" 2>/dev/null || true
  fi
  git -C "$REPO" worktree remove --force "$WORK/old" 2>/dev/null || true
  if [ "$status" -ne 0 ]; then
    echo "--- last lines of the node's log"
    tail -n 40 "$WORK/node/stdout.log" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT

FREENET_TELEMETRY_ENABLED=false "$FREENET_BIN" network \
  --ws-api-port "$API" \
  --config-dir "$WORK/node/cfg" --data-dir "$WORK/node/data" --log-dir "$WORK/node/log" \
  --disable-auto-update --skip-load-from-network --network-address 127.0.0.1 \
  --is-gateway --network-port "$NET" \
  --public-network-address 127.0.0.1 --public-network-port "$NET" \
  --transport-keypair "$WORK/node/transport_keypair" \
  > "$WORK/node/stdout.log" 2>&1 < /dev/null &
NODE_PID=$!
up=false
for _ in $(seq 1 60); do
  if curl -s -m 2 -o /dev/null "http://127.0.0.1:$API/v1/version"; then up=true; break; fi
  sleep 1
done
[ "$up" = true ] || { echo "the node did not come up" >&2; exit 1; }
echo "node up on API port $API"

git -C "$REPO" worktree add --detach "$WORK/old" "$OLD" >/dev/null
cp "$APPS/river-app/examples/rekey_rehearsal.rs" "$WORK/old/apps/river-app/examples/"
old_delegate=$(b3sum --no-names "$WORK/old/ui/public/contracts/chat_delegate.wasm")
old_room=$(b3sum --no-names "$WORK/old/ui/public/contracts/room_contract.wasm")
echo "older build: chat delegate code hash $old_delegate, room contract $old_room"
grep -q "$old_delegate" "$REPO/legacy_delegates.toml" \
  || { echo "the older build's chat delegate is not an earlier generation here" >&2; exit 1; }
grep -q "$old_room" "$REPO/common/legacy_room_contracts.toml" \
  || { echo "the older build's room contract is not an earlier generation here" >&2; exit 1; }

run_old() { (cd "$WORK/old/apps" && ${CARGO:-cargo} run --locked -q -p river-app --example rekey_rehearsal -- "$1" "127.0.0.1:$API" "$WORK/app"); }
run_new() { (cd "$APPS" && ${CARGO:-cargo} run --locked -q -p river-app --example rekey_rehearsal -- "$1" "127.0.0.1:$API" "$WORK/app"); }

run_old seed
run_new load
run_old seed-late
run_new load-late
echo "rekey rehearsal passed"
