#!/usr/bin/env bash
# The cheap CI proof of River's self-update, Linux only, run from `apps/`:
#
#   scripts/update-ci.sh
#
# Builds river with a throwaway update key and the `test-hooks` feature, serves a signed
# manifest and artifact from python3's http.server on a free port in 40400-40403, and runs
# `river --self-update` from a copy of the binary outside target/. It checks:
#   - a current manifest gives exit 10;
#   - a tampered artifact, an unsigned manifest, a manifest signed by another key, and an
#     older (replayed) manifest each give exit 1 and leave the installed binary byte-identical;
#   - a release-style build (no key) reports the placeholder and exits 12.
# The full run with a real window, swap, rollback and confirmation is scripts/update-e2e.sh
# (manual: it needs Xvfb and two builds). The tests that prove the rest are in
# apps/desktop/src/update/ (run by `cargo test`).
set -euo pipefail
cd "$(dirname "$0")/.."

work="$(mktemp -d)"
server_pid=""
cleanup() { [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null; rm -rf "$work"; }
trap cleanup EXIT

cargo build --locked -p river-desktop --example update_tool
tool="$PWD/target/debug/examples/update_tool"
"$tool" keygen "$work/priv" > "$work/pub"
"$tool" keygen "$work/other" > /dev/null
export RIVER_UPDATE_PUBKEY_HEX="$(cat "$work/pub")"
cargo build --locked -p river-desktop --features test-hooks
unset RIVER_UPDATE_PUBKEY_HEX

port=""
for candidate in 40400 40401 40402 40403; do
    if python3 -I -c 'import socket,sys; socket.socket().bind(("127.0.0.1", int(sys.argv[1])))' "$candidate" 2>/dev/null; then
        port="$candidate"; break
    fi
done
test -n "$port" || { echo "no free port among 40400-40403"; exit 2; }
mkdir -p "$work/www" "$work/install" "$work/home"
python3 -I -m http.server "$port" --bind 127.0.0.1 --directory "$work/www" > "$work/http.log" 2>&1 &
server_pid=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$port/" > /dev/null && break; sleep 0.1; done

install="$work/install/river"
cp target/debug/river "$install"
before="$(sha256sum "$install" | cut -d' ' -f1)"
current="$("$install" --version | awk '{print $2}')"
# Not HOME: cargo and rustup live there.
export XDG_DATA_HOME="$work/home/data" XDG_CONFIG_HOME="$work/home/config"
export RIVER_UPDATE_MANIFEST_URL="http://127.0.0.1:$port/manifest.json"
arch="linux-$(uname -m)"
fails=0

publish() { # version sequence [key]
    local version="$1" seq="$2" key="${3:-$work/priv}"
    rm -rf "$work/pkg"; mkdir -p "$work/pkg/River"; cp "$install" "$work/pkg/River/river"
    tar -czf "$work/www/art.tar.gz" -C "$work/pkg" River
    "$tool" sign --key "$key" --version "$version" --sequence "$seq" --out "$work/www" \
        --artifact "$arch=http://127.0.0.1:$port/art.tar.gz=$work/www/art.tar.gz" > /dev/null
}
run() { set +e; "$install" --self-update > "$work/out.log" 2>&1; code=$?; set -e; echo "$code"; }
check() { # name got want
    if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got $2, want $3"; sed 's/^/     | /' "$work/out.log"; fails=$((fails+1)); fi
}
untouched() { check "$1: installed binary byte-identical" "$(sha256sum "$install" | cut -d' ' -f1)" "$before"; }
fresh_state() { rm -rf "$XDG_DATA_HOME"; }

fresh_state; publish "$current" 1000
check "current version: nothing to do" "$(run)" 10; untouched "current"

fresh_state; publish 99.0.0 1000
python3 -I -c 'import sys; p=sys.argv[1]; b=bytearray(open(p,"rb").read()); b[-20]^=0xff; open(p,"wb").write(b)' "$work/www/art.tar.gz"
check "tampered artifact refused" "$(run)" 1; untouched "tampered artifact"

fresh_state; publish 99.0.0 1000; rm "$work/www/manifest.json.sig"
check "unsigned manifest refused" "$(run)" 1; untouched "unsigned"

fresh_state; publish 99.0.0 1000 "$work/other"
check "manifest signed by another key refused" "$(run)" 1; untouched "wrong key"

fresh_state; publish "$current" 2000
check "newer sequence accepted" "$(run)" 10
publish 99.0.0 1500
check "replayed older manifest refused" "$(run)" 1; untouched "replay"

# A build with no key (what a release build is without RIVER_UPDATE_PUBKEY_HEX) says so.
# Empty variables (an undefined workflow variable expands to "") count as unset.
RIVER_UPDATE_PUBKEY_HEX= RIVER_UPDATE_MIN_VERSION= RIVER_UPDATE_MIN_SEQUENCE= \
    cargo build --locked -p river-desktop > /dev/null 2>&1
set +e; ./target/debug/river --update-status > "$work/out.log" 2>&1; code=$?; set -e
check "a build without an update key reports the placeholder" "$code" 12
# A key that is set and malformed is a build error, not a silent placeholder.
set +e; RIVER_UPDATE_PUBKEY_HEX=nothex cargo build --locked -p river-desktop > "$work/out.log" 2>&1; code=$?; set -e
check "a malformed RIVER_UPDATE_PUBKEY_HEX fails the build" "$([[ $code -ne 0 ]] && grep -q "not 64 hex digits" "$work/out.log" && echo failed)" failed
cargo build --locked -p river-desktop > /dev/null 2>&1

[[ $fails -eq 0 ]] && echo "update CI: PASS" || { echo "update CI: $fails FAILED"; exit 1; }
