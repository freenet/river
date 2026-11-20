#!/usr/bin/env bash
# End-to-end test of the native app on Freenet nodes River never ran on.
#
# Starts two fresh Freenet nodes in network mode on loopback (a gateway and a peer that joins
# it, each with its own empty config and data directories, so neither has River's web UI or
# chat delegate), then runs apps/river-app/examples/fresh_node_e2e.rs against them: on each
# node the app approves its grant, registers the chat delegate and reads an empty room list;
# the app on the gateway creates a room and an invitation, the app on the peer joins from
# it, each receives the other's message, each sends the other a direct message (an empty one
# is refused) that the other opens with its own key and the sender reads back as its own, and
# each changes its own nickname and the other sees
# it; the owner makes the joiner a deputy, whose app shows the Deputy tag, and revokes it. Last,
# the example asks each node's chat delegate for
# the room's signing key and requires it to be that app's own member key (the app's background
# sweep stores it). Then the owner's app changes the room's name, description and a limit (a new
# signed configuration) and the other app shows them, and the other app's member is refused when
# it tries the same. Then, on the gateway, a pointer record signed by a throwaway key names a
# newer chat delegate, and a third app that trusts that key must show the room read-only and
# write nothing (the record goes to the local test node only, never to River's addresses).
#
# Then the identity moves: on the gateway the app creates a room and exports its identity, the
# app on the peer imports it (adding the room), shows it and sends in it as that same member; a
# bad token and a replace nobody asked for are refused, and a replace the user confirmed makes
# the peer's app write as the gateway app's member in a room it already held.
#
# Last, the app is pointed at the gateway as a node it was told about, on a port other than 7509
# (apps/river-app/examples/remote_node_e2e.rs): through the saved setting, through $FREENET_NODE
# (which must beat a wrong saved address), and through a plain TCP forwarder in front of the node,
# which is what an SSH tunnel to a node on another machine looks like to it. Each time the setup
# step reaches Connected without touching a Freenet on this computer, the grant is approved on the
# page at the chosen address, and the app creates a room and sees its own message. The forwarder
# listens on the TCP port with the number of the peer's network port (that one is UDP), so the
# whole run stays inside the four ports.
#
# Usage: apps/scripts/fresh-node-e2e.sh <work dir>
#   FREENET_BIN  the freenet binary to run (default: freenet on PATH)
#   E2E_PORT_BASE  first of the four ports used (default 39400): two network, two API
#   POINTER_WASM   the frozen pointer contract WASM (default: fetched from freenet-migrate at
#                  a pinned commit; the example checks its hash either way)
#
# Needs openssl (for the gateway's transport key) and curl. Nothing outside <work dir> is
# written, apart from cargo's build output.
set -euo pipefail

WORK="${1:?usage: $0 <work dir>}"
FREENET_BIN="${FREENET_BIN:-freenet}"
BASE="${E2E_PORT_BASE:-39400}"
GW_NET=$BASE PEER_NET=$((BASE + 1)) GW_API=$((BASE + 2)) PEER_API=$((BASE + 3))
APPS="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
for d in gw peer apps; do
  if [ -e "$WORK/$d" ]; then
    echo "$WORK/$d exists; use an empty work dir so the nodes start fresh" >&2
    exit 1
  fi
done
mkdir -p "$WORK/gw/cfg" "$WORK/gw/data" "$WORK/gw/log" \
  "$WORK/peer/cfg" "$WORK/peer/data" "$WORK/peer/log" "$WORK/apps"

echo "freenet: $("$FREENET_BIN" --version | head -n1)"

# The pointer contract is frozen: freenet-migrate pins its code hash, and the example refuses
# any other bytes, so where it comes from does not matter for trust.
POINTER_WASM_COMMIT=6b724f1badeed1891f4c09ddb1d6aa2c0921508f
if [ -z "${POINTER_WASM:-}" ]; then
  POINTER_WASM="$WORK/pointer-v1.wasm"
  curl -sfL --retry 3 --retry-all-errors -o "$POINTER_WASM" \
    "https://raw.githubusercontent.com/freenet/freenet-migrate/$POINTER_WASM_COMMIT/contracts/pointer-contract/pointer-v1.wasm"
fi
export POINTER_WASM

# The gateway's transport key, made here so the peer can be told its public half.
openssl genpkey -algorithm X25519 -out "$WORK/gw/x25519.pem" 2>/dev/null
openssl pkey -in "$WORK/gw/x25519.pem" -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n' \
  > "$WORK/gw/transport_keypair"
GW_PUB="$(openssl pkey -in "$WORK/gw/x25519.pem" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')"
test "${#GW_PUB}" -eq 64

PIDS=()
cleanup() {
  status=$?
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${PIDS[@]}"; do
    for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill -9 "$pid" 2>/dev/null || true
  done
  if [ "$status" -ne 0 ]; then
    for n in gw peer; do
      echo "--- last lines of the $n node's log"
      tail -n 40 "$WORK/$n/stdout.log" 2>/dev/null || true
      tail -n 40 "$WORK/$n/log/"freenet.*.log 2>/dev/null | grep -v '^$' | tail -n 40 || true
    done
  fi
  exit "$status"
}
trap cleanup EXIT

start() {
  local name=$1 api=$2
  shift 2
  FREENET_TELEMETRY_ENABLED=false "$FREENET_BIN" network \
    --ws-api-port "$api" \
    --config-dir "$WORK/$name/cfg" --data-dir "$WORK/$name/data" --log-dir "$WORK/$name/log" \
    --disable-auto-update --skip-load-from-network --network-address 127.0.0.1 "$@" \
    > "$WORK/$name/stdout.log" 2>&1 < /dev/null &
  PIDS+=($!)
  for _ in $(seq 1 60); do
    if curl -s -m 2 -o /dev/null "http://127.0.0.1:$api/v1/version"; then
      echo "$name node up on API port $api"
      return 0
    fi
    sleep 1
  done
  echo "$name node did not come up" >&2
  return 1
}

start gw "$GW_API" --is-gateway --network-port "$GW_NET" \
  --public-network-address 127.0.0.1 --public-network-port "$GW_NET" \
  --transport-keypair "$WORK/gw/transport_keypair"
start peer "$PEER_API" --network-port "$PEER_NET" --gateway "127.0.0.1:$GW_NET,$GW_PUB"

# Wait for the peer to join the gateway before any room is created.
joined=false
for _ in $(seq 1 90); do
  if grep -qh "open_conns=[1-9]\|open_connections=[1-9]" "$WORK/peer/log/"freenet.*.log 2>/dev/null; then
    joined=true
    echo "peer connected to the gateway"
    break
  fi
  sleep 1
done
if [ "$joined" != true ]; then
  echo "the peer never connected to the gateway" >&2
  exit 1
fi

# Neither node has River's chat delegate before the app runs.
for n in gw peer; do
  if find "$WORK/$n/data/delegates" -type f 2>/dev/null | grep -q .; then
    echo "the $n node already holds a delegate; it is not fresh" >&2
    exit 1
  fi
done

(cd "$APPS" && cargo run --locked -q -p river-app --example fresh_node_e2e -- \
  "127.0.0.1:$GW_API" "127.0.0.1:$PEER_API" "$WORK/apps")

# The app registered the chat delegate on both nodes.
for n in gw peer; do
  if ! find "$WORK/$n/data/delegates" -type f | grep -q .; then
    echo "no delegate stored on the $n node after the run" >&2
    exit 1
  fi
done

# The app pointed at the gateway as a node it was told about, on a port other than 7509.
FWD=$PEER_NET   # TCP; the peer's network port of that number is UDP
for mode in saved env tunnel; do
  mkdir -p "$WORK/remote-$mode"
  if [ "$mode" = env ]; then
    (cd "$APPS" && FREENET_NODE="127.0.0.1:$GW_API" cargo run --locked -q -p river-app \
      --example remote_node_e2e -- env "127.0.0.1:$GW_API" "$WORK/remote-$mode")
  else
    (cd "$APPS" && env -u FREENET_NODE cargo run --locked -q -p river-app \
      --example remote_node_e2e -- "$mode" "127.0.0.1:$GW_API" "$WORK/remote-$mode" "127.0.0.1:$FWD")
  fi
done
echo "fresh-node e2e passed"
