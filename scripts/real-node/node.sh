#!/usr/bin/env bash
# An isolated Freenet node for checking River against a real node, without
# touching your own node (default port 7509) or its data.
#
#   scripts/real-node/node.sh start [local|joining]   start (or restart) it
#   scripts/real-node/node.sh stop                    stop it (only ours)
#   scripts/real-node/node.sh status
#   scripts/real-node/node.sh publish [--skip-build]  build + publish River to it
#   scripts/real-node/node.sh url                     River's URL on it
#   scripts/real-node/node.sh reset                   stop and delete its data
#
# Modes:
#   local    a `freenet local` node: ready as soon as its API answers.
#   joining  a `freenet network` peer whose only gateway is unreachable, so it
#            never joins: contract pages answer 503 "Connecting to Freenet",
#            the state a restarted network peer is in until it has rejoined.
#
# Environment:
#   FREENET_BIN   node binary (default: `freenet` on PATH). Use a RELEASE build:
#                 a debug build deletes its --config-dir/--data-dir on SIGTERM,
#                 so the restart finds nothing. This script refuses debug builds.
#   FDEV_BIN      fdev to publish with (default: `fdev` on PATH). It must speak
#                 the node's protocol; an fdev built from the same tree does.
#   NODE_DIR      state dir (default: $TMPDIR/river-real-node). Data persists
#                 across stop/start; `reset` wipes it.
#   NODE_PORT     WebSocket API port (default 7520). NETWORK_PORT (default 31340).
set -euo pipefail

cd "$(dirname "$0")/../.."
NODE_DIR="${NODE_DIR:-${TMPDIR:-/tmp}/river-real-node}"
NODE_DIR="${NODE_DIR%/}"
PORT="${NODE_PORT:-7520}"
NET_PORT="${NETWORK_PORT:-31340}"
BIN="${FREENET_BIN:-$(command -v freenet || true)}"
START_TIMEOUT_S=10

die() { echo "node.sh: $*" >&2; exit 1; }

api_up() { curl -s -o /dev/null -m 1 -w '%{http_code}' "http://127.0.0.1:$PORT/v1/version" | grep -q 200; }

our_pid() {
    [ -f "$NODE_DIR/pid" ] || return 1
    local pid; pid=$(cat "$NODE_DIR/pid")
    # Only a process started with OUR config dir is ours to stop.
    ps -p "$pid" -o command= 2>/dev/null | grep -qF -- "--config-dir $NODE_DIR/config" || return 1
    echo "$pid"
}

stop_node() {
    local pid
    if pid=$(our_pid); then
        kill "$pid"
        for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
        kill -0 "$pid" 2>/dev/null && kill -9 "$pid"
        echo "stopped $pid"
    fi
    rm -f "$NODE_DIR/pid"
}

start_node() {
    local mode="${1:-local}"
    [ -n "$BIN" ] && [ -x "$BIN" ] || die "no node binary: set FREENET_BIN to a release build"
    case "$BIN" in */debug/*) die "$BIN is a debug build; it deletes its data dir on stop. Use target/release." ;; esac
    stop_node >/dev/null
    if api_up; then die "port $PORT is already serving another node; set NODE_PORT"; fi
    mkdir -p "$NODE_DIR/config" "$NODE_DIR/data" "$NODE_DIR/log"
    local common=(--ws-api-port "$PORT" --network-port "$NET_PORT" --config-dir "$NODE_DIR/config"
        --data-dir "$NODE_DIR/data" --log-dir "$NODE_DIR/log" --disable-auto-update)
    local args
    case "$mode" in
        local) args=(local "${common[@]}") ;;
        joining) args=(network "${common[@]}" --skip-load-from-network
            --gateway "127.0.0.1:39999,0000000000000000000000000000000000000000000000000000000000000001") ;;
        *) die "unknown mode $mode (local|joining)" ;;
    esac
    # Started directly (not through a subshell) so the pid is the node's.
    nohup "$BIN" "${args[@]}" >>"$NODE_DIR/stdout.log" 2>&1 &
    echo $! >"$NODE_DIR/pid"
    echo "$mode" >"$NODE_DIR/mode"
    local deadline=$((SECONDS + START_TIMEOUT_S))
    until api_up; do
        if ! kill -0 "$(cat "$NODE_DIR/pid")" 2>/dev/null; then
            tail -5 "$NODE_DIR/stdout.log" >&2
            die "node exited during start"
        fi
        [ $SECONDS -lt $deadline ] || { tail -5 "$NODE_DIR/stdout.log" >&2; die "API not up after ${START_TIMEOUT_S}s"; }
        sleep 0.2
    done
    echo "node up ($mode, pid $(cat "$NODE_DIR/pid"), port $PORT, $("$BIN" --version 2>/dev/null | head -1))"
}

contract_id() { cat test-contract/contract-id.txt 2>/dev/null || true; }

case "${1:-}" in
    start) start_node "${2:-local}" ;;
    stop) stop_node ;;
    status)
        if pid=$(our_pid); then echo "running: pid $pid, mode $(cat "$NODE_DIR/mode" 2>/dev/null), port $PORT"
        else echo "not running"; fi ;;
    publish)
        api_up || die "start the node first"
        [ "$(cat "$NODE_DIR/mode" 2>/dev/null)" = local ] || die "publish to a local-mode node"
        FDEV="${FDEV_BIN:-$(command -v fdev || true)}"
        [ -n "$FDEV" ] || die "no fdev: set FDEV_BIN"
        # local-republish.sh calls `fdev` from PATH; it rebuilds target/dx
        # without example data unless --skip-build.
        PATH="$(dirname "$FDEV"):$PATH" ./scripts/local-republish.sh --port "$PORT" "${@:2}" >"$NODE_DIR/publish.log" 2>&1 \
            || { tail -20 "$NODE_DIR/publish.log" >&2; die "publish failed"; }
        echo "published: http://127.0.0.1:$PORT/v1/contract/web/$(contract_id)/" ;;
    url) echo "http://127.0.0.1:$PORT/v1/contract/web/$(contract_id)/" ;;
    reset) stop_node; rm -rf "$NODE_DIR"; echo "removed $NODE_DIR" ;;
    *) sed -n '2,25p' "$0"; exit 1 ;;
esac
