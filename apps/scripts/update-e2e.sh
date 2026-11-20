#!/usr/bin/env bash
# End-to-end test of River's self-update with REAL binaries, a real local HTTP server and a real
# (virtual) display.
#
#   update-e2e.sh <old river> <new river> <update_tool> <work dir>
#
# - <old river>: a River build of version A whose compiled-in update key is a TEST key
#   (build with RIVER_UPDATE_PUBKEY_HEX set to the public key that `update_tool keygen` prints).
# - <new river>: the same source built as a higher version B with the same test key.
# - <update_tool>: target/debug/examples/update_tool.
# - <work dir>: empty; everything is created inside it.
#
# The Rivers must be built with `--features test-hooks` if they are release builds (debug builds
# have the manifest-URL override anyway).
#
# Recipe for the two builds (the version is the workspace version in apps/Cargo.toml):
#   key=$(update_tool keygen work/priv)   # once; the script signs with work/priv
#   RIVER_UPDATE_PUBKEY_HEX=$key cargo build -p river-desktop --features test-hooks   # as 0.1.0 -> old
#   (edit version to 0.1.1)  RIVER_UPDATE_PUBKEY_HEX=$key cargo build -p river-desktop --features test-hooks   # new
#
# Uses one free TCP port among 40400-40403 and X display :47 only. Never touches the real node (7509), the user's
# River data (HOME and XDG dirs point into the work dir) or any release secret: the key is a
# throwaway made for this run.
set -uo pipefail

OLD="$(readlink -f "${1:?old river}")"
NEW="$(readlink -f "${2:?new river}")"
TOOL="$(readlink -f "${3:?update_tool}")"
WORK="$(mkdir -p "${4:?work dir}" && readlink -f "$4")"
PORT=""
for candidate in 40400 40401 40402 40403; do
    if python3 -I -c 'import socket,sys; socket.socket().bind(("127.0.0.1", int(sys.argv[1])))' "$candidate" 2>/dev/null; then
        PORT="$candidate"; break
    fi
done
[[ -n "$PORT" ]] || { echo "no free port among 40400-40403"; exit 2; }
DISPLAY_NUM=47
export DISPLAY=":$DISPLAY_NUM"
export HOME="$WORK/home" XDG_DATA_HOME="$WORK/data" XDG_CONFIG_HOME="$WORK/config"
mkdir -p "$HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$WORK/www" "$WORK/stage"
export RIVER_UPDATE_MANIFEST_URL="http://127.0.0.1:$PORT/manifest.json"

OLD_VER="$("$OLD" --version | awk '{print $2}')"
NEW_VER="$("$NEW" --version | awk '{print $2}')"
INSTALL="$WORK/install/river"
fails=0
pids=()

cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT

[[ -f "$WORK/priv" ]] || { echo "make the test key first: $TOOL keygen $WORK/priv  (its output is the key to build both Rivers with)"; exit 2; }
[[ -f "$WORK/priv2" ]] || "$TOOL" keygen "$WORK/priv2" > /dev/null

python3 -I -m http.server "$PORT" --bind 127.0.0.1 --directory "$WORK/www" > "$WORK/http.log" 2>&1 &
pids+=("$!")
Xvfb ":$DISPLAY_NUM" -nolisten tcp -screen 0 1024x768x24 > "$WORK/xvfb.log" 2>&1 &
pids+=("$!")
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$PORT/" > /dev/null && break; sleep 0.1; done

sha() { sha256sum "$1" | cut -d' ' -f1; }
OLD_SHA="$(sha "$OLD")"; NEW_SHA="$(sha "$NEW")"

fresh_install() { # reset install dir and the updater's state
    rm -rf "$WORK/install" "$XDG_DATA_HOME" "$WORK/www"/*; mkdir -p "$WORK/install" "$XDG_DATA_HOME" "$WORK/www"
    cp "$OLD" "$INSTALL"
}

# package <binary> <out tar.gz>: the layout package-linux.sh makes.
package() {
    rm -rf "$WORK/stage"/*; mkdir -p "$WORK/stage/River"; install -m 0755 "$1" "$WORK/stage/River/river"
    tar -czf "$2" -C "$WORK/stage" River
}

# publish <version> <sequence> <binary> [signing key]: serve a signed manifest and artifact.
publish() {
    local version="$1" seq="$2" bin="$3" key="${4:-$WORK/priv}"
    package "$bin" "$WORK/www/art-$version.tar.gz"
    "$TOOL" sign --key "$key" --version "$version" --sequence "$seq" --out "$WORK/www" \
        --artifact "linux-$(uname -m)=http://127.0.0.1:$PORT/art-$version.tar.gz=$WORK/www/art-$version.tar.gz" \
        --notes "e2e" > /dev/null || { echo "could not sign"; exit 2; }
}

# run_update: the installed River checks and updates. Prints its exit code.
run_update() { "$INSTALL" --self-update > "$WORK/last.log" 2>&1; echo $?; }

expect() { # name actual expected
    if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails+1)); cat "$WORK/last.log" 2>/dev/null | sed 's/^/     | /'; fi
}
expect_untouched() { expect "$1: installed binary byte-identical" "$(sha "$INSTALL")" "$OLD_SHA"; }

echo "old=$OLD_VER new=$NEW_VER"

# 1. A tampered artifact: signed manifest, same-size artifact with one byte changed.
fresh_install; publish "$NEW_VER" 1000 "$NEW"
python3 -I -c "
import sys
p=sys.argv[1]; b=bytearray(open(p,'rb').read()); b[-20]^=0xff; open(p,'wb').write(b)" "$WORK/www/art-$NEW_VER.tar.gz"
expect "tampered artifact refused" "$(run_update)" 1; expect_untouched "tampered artifact"
grep -q "signed checksum" "$WORK/last.log" && echo "ok   tampered artifact: said why" || { echo "FAIL tampered artifact: no reason"; fails=$((fails+1)); }

# 2. A manifest signed by another key.
fresh_install; publish "$NEW_VER" 1000 "$NEW" "$WORK/priv2"
expect "bad signature refused" "$(run_update)" 1; expect_untouched "bad signature"

# 3. No signature at all.
fresh_install; publish "$NEW_VER" 1000 "$NEW"; rm "$WORK/www/manifest.json.sig"
expect "unsigned refused" "$(run_update)" 1; expect_untouched "unsigned"

# 4. Downgrade, then replay of an older manifest, then rollback to an older version.
fresh_install; publish "0.0.9" 2000 "$NEW"
expect "downgrade: nothing offered" "$(run_update)" 10; expect_untouched "downgrade"
publish "$NEW_VER" 1500 "$NEW"
expect "replayed older manifest refused" "$(run_update)" 1; expect_untouched "replay"
publish "0.0.5" 2100 "$NEW"
expect "rollback to an older version refused" "$(run_update)" 1; expect_untouched "rollback"

# 5. A new binary that passes the version check and then dies: rolled back.
fresh_install
printf '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "river %s"; exit 0; fi\nexit 3\n' "$NEW_VER" > "$WORK/broken"
chmod +x "$WORK/broken"; publish "$NEW_VER" 3000 "$WORK/broken"
expect "broken new binary: update fails" "$(run_update)" 1; expect_untouched "broken new binary"
grep -q "went back to $OLD_VER" "$WORK/last.log" && echo "ok   broken new binary: rollback reported" || { echo "FAIL broken new binary: no rollback message"; fails=$((fails+1)); }
expect "broken new binary: probation cleared" "$(grep -c '"probation": null' "$(find "$XDG_DATA_HOME" -name update-state.json)")" 1

# 6. The real thing: verified, swapped, started (a real window on the virtual display), confirmed.
fresh_install; publish "$NEW_VER" 4000 "$NEW"
expect "update succeeds" "$(run_update)" 0
expect "new binary installed" "$(sha "$INSTALL")" "$NEW_SHA"
expect "previous binary kept for rollback" "$(sha "$(find "$XDG_DATA_HOME" -path '*update/previous/*' -type f | head -1)")" "$OLD_SHA"
running=""
for _ in $(seq 40); do
    for d in /proc/[0-9]*; do
        if [[ "$(readlink "$d/exe" 2>/dev/null)" == "$INSTALL" ]]; then running="${d#/proc/}"; fi
    done
    [[ -n "$running" ]] && break; sleep 0.25
done
expect "the new River is running" "$([[ -n "$running" ]] && echo yes || echo no)" yes
sleep 3   # a window running a few seconds is the new binary confirming, then probation ends
expect "still running after the handshake" "$([[ -d /proc/$running ]] && echo yes || echo no)" yes
expect "probation ended (new binary confirmed)" "$(grep -c '"probation": null' "$(find "$XDG_DATA_HOME" -name update-state.json)")" 1
[[ -n "$running" ]] && kill "$running" 2>/dev/null
cp "$INSTALL" "$WORK/installed-new"
expect "the new River finds itself current" "$("$INSTALL" --self-update > "$WORK/last.log" 2>&1; echo $?)" 10

echo; [[ $fails -eq 0 ]] && echo "update e2e: PASS" || echo "update e2e: $fails FAILED"
exit $((fails > 0))
