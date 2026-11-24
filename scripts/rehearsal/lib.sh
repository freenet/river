# Shared helpers for the migration rehearsal. Sourced by run.sh, not run.
# Expects WORK (the rehearsal work dir) to be set by the caller.
# Isolated network-mode gateway: its own dirs, ports and webapp cache, no peers.
# NEVER touches 7509 (production gateway) or 7510/7530.
WS_PORT=${REHEARSAL_WS_PORT:-7891}
NET_PORT=${REHEARSAL_NET_PORT:-7892}
case "$WS_PORT" in 7509|7510|7530|7540|7542) echo "refusing port $WS_PORT" >&2; exit 1;; esac
URL="ws://127.0.0.1:$WS_PORT/v1/contract/command?encodingProtocol=native"
export RIVERCTL_NO_VERSION_CHECK=1
# The node binary. Prefer a pinned copy over whatever is on PATH, so two
# rehearsals of different commits run against the same node; run.sh records
# its version and BLAKE3 in the summary.
FREENET_BIN=${FREENET_BIN:-$(command -v freenet || true)}
[ -x "$FREENET_BIN" ] || { echo "no freenet binary: set FREENET_BIN" >&2; exit 1; }
# Rehearsal nodes must never report to the central collector: their instant
# NotFounds were once read as a release regression. Telemetry and the
# OTel exporter are switched off, and the endpoint points at a local port
# nothing listens on, so a misparsed switch still cannot reach the collector.
# start_node refuses to continue unless the node logs that telemetry is off.
TELEMETRY_SINK=http://127.0.0.1:9

NODE_PID=
NODE_DIR=

# Telemetry-off PROOF beyond the node's own log line: every socket owned by the
# node PID must be local (listener, loopback peer). Any other remote peer is a leak.
telemetry_proof() {
  local when=$1 out bad
  out=$(ss -H -tunap 2>/dev/null | grep "pid=$NODE_PID," | awk '{print $1,$2,$5,"->",$6}')
  { echo "[$when] $(date +%T) pid $NODE_PID"; echo "$out"; } >> "$NODE_DIR/netproof.txt"
  bad=$(echo "$out" | awk 'NF{print $5}' | grep -vE '^(127\.0\.0\.1:|\[::1\]:|0\.0\.0\.0:\*|\*:\*|\[::\]:\*)' | tr '\n' ' ')
  [ -z "$bad" ] || echo "TELEMETRY LEAK: node $NODE_PID has non-loopback sockets: $bad" | tee -a "$WORK/TELEMETRY_LEAK" >&2
}
stop_node() {
  [ -n "$NODE_PID" ] || return 0
  telemetry_proof stop
  kill "$NODE_PID" 2>/dev/null || true
  for _ in $(seq 1 30); do kill -0 "$NODE_PID" 2>/dev/null || break; sleep 1; done
  if kill -0 "$NODE_PID" 2>/dev/null; then kill -9 "$NODE_PID" 2>/dev/null || true; sleep 1; fi
  if kill -0 "$NODE_PID" 2>/dev/null; then echo "node $NODE_PID did not stop" >&2; exit 1; fi
  NODE_PID=
}

start_node() {
  local d=$1
  [ -n "${KEEP_DATA:-}" ] || rm -rf "$d"; mkdir -p "$d"/{config,data,log,cache}
  if ss -ltn | grep -qE "[:.]$WS_PORT "; then echo "port $WS_PORT is in use" >&2; exit 1; fi
  FREENET_TELEMETRY_ENABLED=false FREENET_WEBAPP_CACHE_DIR=$d/cache "$FREENET_BIN" network \
    --telemetry-endpoint "$TELEMETRY_SINK" --otel-telemetry-enabled=false --is-gateway --skip-load-from-network \
    --disable-auto-update --network-address 127.0.0.1 --public-network-address 127.0.0.1 \
    --public-network-port "$NET_PORT" --network-port "$NET_PORT" \
    --ws-api-address 127.0.0.1 --ws-api-port "$WS_PORT" \
    --config-dir "$d/config" --data-dir "$d/data" --log-dir "$d/log" > "$d/stdout.log" 2>&1 < /dev/null &
  NODE_PID=$!
  NODE_DIR=$d
  for _ in $(seq 1 90); do curl -s -o /dev/null "http://127.0.0.1:$WS_PORT/" && break; sleep 1; done
  local owner
  owner=$(ss -ltnp | grep -E "127.0.0.1:$WS_PORT " | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2 || true)
  [ "$owner" = "$NODE_PID" ] || { echo "node did not start or port $WS_PORT is not ours (see $d/stdout.log)" >&2; exit 1; }
  local tl=
  for _ in $(seq 1 20); do
    tl=$(cat "$d"/log/*.log "$d"/stdout.log 2>/dev/null | grep -oE "Telemetry (reporting is disabled|disabled[^\"]*)|Telemetry[^\"]*endpoint[^\"]*" | head -1)
    [ -n "$tl" ] && break; sleep 0.5
  done
  case "$tl" in
    "Telemetry reporting is disabled") ;;
    *) echo "node $NODE_PID did not log that telemetry is disabled (saw: '${tl:-nothing}'); stopping it" >&2; stop_node; exit 1 ;;
  esac
  telemetry_proof start
  echo "   node pid $NODE_PID ($("$FREENET_BIN" --version | awk "NR==1")), network mode, isolated gateway, ws $WS_PORT, telemetry off (node logged: $tl)"
}

# Count of peer connections the node made: must stay 0 (isolation evidence).
peer_connections() {
  cat "$NODE_DIR"/log/*.log "$NODE_DIR"/stdout.log 2>/dev/null | grep -c "Adding connection to peer" || true
}

# Delegate secret dirs the node holds, as hex-less names (the node's own layout).
secret_dirs() {
  find "$NODE_DIR" -type d -path '*secrets*' -mindepth 1 2>/dev/null | sed "s|$NODE_DIR/||" | sort
}

progress() {
  echo "$(date '+%F %H:%M') [rehearsal] $*" >> "$WORK/progress.md"
  echo "$*"
}

# The node names a delegate's secret dir by the base58 of its key.
key_dirname() {
  python3 -c "
import sys
b=bytes.fromhex(sys.argv[1]); a='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
n=int.from_bytes(b,'big'); s=''
while n: n,r=divmod(n,58); s=a[r]+s
print('1'*(len(b)-len(b.lstrip(b'\0')))+s)" "$1"
}

# "<dir> <files>" for every delegate secret dir on the node.
secret_listing() {
  local out=$1 base=$NODE_DIR/data/secrets
  : > "$out"
  [ -d "$base" ] || return 0
  find "$base" -mindepth 1 -maxdepth 2 -type d | while read -r dd; do
    local n; n=$(find "$dd" -maxdepth 1 -type f | wc -l)
    [ "$n" -gt 0 ] && echo "${dd#$base/}:$n" >> "$out"
  done
  sort -o "$out" "$out"
}
