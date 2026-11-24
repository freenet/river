#!/usr/bin/env bash
# Rehearsal of a re-key for the native app (freenet/river#889), on one isolated Freenet node.
# Run it before every River publish that changes chat_delegate.wasm or room_contract.wasm, and
# before the desktop release that carries them (.claude/rules/river-publish.md,
# .claude/rules/desktop-release.md).
#
# Each OLDER River app given (built from that commit, whose chat delegate and room contract are
# earlier generations than this checkout's) seeds the node under its own tag: it creates three
# rooms, sends a message in one and leaves another. Then this checkout's app runs on the same
# node and must show every kept room (with the message, which lives under the older room
# contract), none left, hold them in its own chat delegate, and send under the current room
# contract. Then the newest older app writes a new room (a late write from an earlier
# generation) and this checkout's app, started again, must show it too.
# apps/river-app/examples/rekey_rehearsal.rs is the driver; it is copied into each older checkout.
#
# Usage: apps/scripts/rekey-rehearsal.sh <work dir> [<old commit> ...]
#   With no commit: the commit before the last change to ui/public/contracts/chat_delegate.wasm
#   or room_contract.wasm (the outgoing generation, which is what a re-key publish strands; it
#   needs the git history, so not a shallow clone). Give several, oldest first, to rehearse a
#   skipped release too, e.g. `14d9632d e9dae028^`. Nothing enforces running it: it is a step
#   of the publish and release procedures.
#   FREENET_BIN          the freenet binary to run (default: freenet on PATH)
#   REHEARSAL_PORT_BASE  first of the two ports used (default 39500): network, API
#   CARGO                the cargo command (default: cargo), e.g. a wrapper that takes a lock
#
# Needs: git, cargo, freenet (0.2.156 or later: the app grant), b3sum, openssl, curl, od.
# Each older commit is a full build of apps/ (several minutes); each step waits up to 4 minutes.
# The node runs on loopback with no peers, its own empty directories and telemetry off. Nothing
# outside <work dir> is written, apart from cargo's build output for this checkout.
set -euo pipefail

WORK="${1:?usage: $0 <work dir> [<old commit> ...]}"
shift
APPS="$(cd "$(dirname "$0")/.." && pwd)"
REPO="$(cd "$APPS/.." && pwd)"
if [ "$#" -eq 0 ]; then
  # The build before the last change to either WASM: the outgoing generation of whichever
  # re-keyed last. Needs the history (not a shallow clone).
  last=$(git -C "$REPO" log -1 --format=%H -- ui/public/contracts/chat_delegate.wasm \
    ui/public/contracts/room_contract.wasm)
  set -- "$last^"
fi
FREENET_BIN="${FREENET_BIN:-freenet}"
BASE="${REHEARSAL_PORT_BASE:-39500}"
NET=$BASE API=$((BASE + 1))
for tool in git b3sum openssl curl od "$FREENET_BIN"; do
  command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
for d in node old0 app; do
  if [ -e "$WORK/$d" ]; then
    echo "$WORK/$d exists; use an empty work dir" >&2
    exit 1
  fi
done
mkdir -p "$WORK/node/cfg" "$WORK/node/data" "$WORK/node/log" "$WORK/app"
echo "freenet: $("$FREENET_BIN" --version | head -n1)"
echo "app: $(git -C "$REPO" rev-parse HEAD)$(git -C "$REPO" diff --quiet HEAD -- apps || echo " (with uncommitted changes in apps/)")"

openssl genpkey -algorithm X25519 -out "$WORK/node/x25519.pem" 2>/dev/null
openssl pkey -in "$WORK/node/x25519.pem" -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n' \
  > "$WORK/node/transport_keypair"

NODE_PID=""
TREES=()
cleanup() {
  status=$?
  if [ -n "$NODE_PID" ]; then
    kill "$NODE_PID" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$NODE_PID" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$NODE_PID" 2>/dev/null || true
  fi
  for t in "${TREES[@]}"; do
    git -C "$REPO" worktree remove --force "$t" 2>/dev/null || true
  done
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

run_in() { (cd "$1" && ${CARGO:-cargo} run --locked -q -p river-app --example rekey_rehearsal -- "$2" "127.0.0.1:$API" "$WORK/app" "$3"); }

TAGS=()
i=0
for old in "$@"; do
  tree="$WORK/old$i"
  git -C "$REPO" worktree add --detach "$tree" "$old" >/dev/null
  TREES+=("$tree")
  cp "$APPS/river-app/examples/rekey_rehearsal.rs" "$tree/apps/river-app/examples/"
  delegate_hash=$(b3sum --no-names "$tree/ui/public/contracts/chat_delegate.wasm")
  room_hash=$(b3sum --no-names "$tree/ui/public/contracts/room_contract.wasm")
  echo "older build $old: chat delegate $delegate_hash, room contract $room_hash"
  current_delegate=$(b3sum --no-names "$REPO/ui/public/contracts/chat_delegate.wasm")
  current_room=$(b3sum --no-names "$REPO/ui/public/contracts/room_contract.wasm")
  if [ "$delegate_hash" != "$current_delegate" ]; then
    grep -q "$delegate_hash" "$REPO/legacy_delegates.toml" \
      || { echo "$old's chat delegate is not an earlier generation here" >&2; exit 1; }
  fi
  if [ "$room_hash" != "$current_room" ]; then
    grep -q "$room_hash" "$REPO/common/legacy_room_contracts.toml" \
      || { echo "$old's room contract is not an earlier generation here" >&2; exit 1; }
  fi
  tag="G$i"
  run_in "$tree/apps" seed "$tag"
  TAGS+=("$tag")
  i=$((i + 1))
done
all=$(IFS=,; echo "${TAGS[*]}")
last="${TAGS[${#TAGS[@]} - 1]}"
run_in "$APPS" load "$all"
run_in "${TREES[${#TREES[@]} - 1]}/apps" seed-late "$last"
run_in "$APPS" load-late "$all"
echo "rekey rehearsal passed"
