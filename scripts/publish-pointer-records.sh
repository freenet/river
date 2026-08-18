#!/bin/bash
# Publish River's signed pointer records to the Freenet network.
#
# Run this from `main`, after the PR carrying the signed records has merged and
# CI is green on that exact SHA. Signing is offline and happens in the PR
# (scripts/sign-pointer-records.sh); this step is the network half.
#
# ## The checks below are the point of this script
#
# Every one of them can fail, and each exists because of a specific way a
# publish can look successful from here while being useless or invisible to
# everyone else. In particular a PUT at an already-used version is a NO-OP
# SUCCESS — the pointer contract deliberately does not error on a stale update,
# because erroring would turn routine anti-entropy from a peer that is merely
# behind into a merge failure. So the network will never tell you your publish
# was ignored. That is why this script re-reads afterwards rather than trusting
# "published successfully".
#
# Usage:
#   scripts/publish-pointer-records.sh --node-port 7599 [--dry-run] [--yes]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="pointer-records.toml"
WEB_CONTAINER_KEY="raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv"

cd "$REPO_ROOT"

die() { echo "ERROR: $*" >&2; exit 1; }
say() { echo "  $*"; }

PORT=""
DRY_RUN=0
ASSUME_YES=0
POINTER_WASM="${POINTER_WASM:-}"
while [ $# -gt 0 ]; do
    case "$1" in
        --node-port) PORT="${2:?--node-port needs a value}"; shift 2 ;;
        --pointer-wasm) POINTER_WASM="${2:?--pointer-wasm needs a value}"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        --yes) ASSUME_YES=1; shift ;;
        *) die "unknown argument '$1'" ;;
    esac
done
[ -n "$PORT" ] || die "--node-port is required (the node to publish THROUGH)"

# 7509 is the PRODUCTION gateway. Publishing through it is not the intended
# path and testing against it is explicitly out of bounds.
[ "$PORT" != "7509" ] || die "port 7509 is the production gateway — publish through your own node"

command -v fdev >/dev/null 2>&1 || die "fdev not found"
command -v b3sum >/dev/null 2>&1 || die "b3sum not found"
command -v pointer-record >/dev/null 2>&1 || die "pointer-record not found"
[ -f "$TOML_PATH" ] || die "$TOML_PATH not found"

field_of_record() {
    awk -v want="$1" -v key="$2" '
        /^\[\[record\]\]/ { i++; next }
        i == want && $0 ~ "^"key"[ \t]*=" {
            sub("^"key"[ \t]*=[ \t]*", ""); gsub(/^"|"$/, ""); print; exit
        }
    ' "$TOML_PATH"
}
top_level_field() {
    awk -v key="$1" '
        /^\[\[record\]\]/ { exit }
        $0 ~ "^"key"[ \t]*=" {
            sub("^"key"[ \t]*=[ \t]*", ""); gsub(/^"|"$/, ""); print; exit
        }
    ' "$TOML_PATH"
}

AUTHOR_VK="$(top_level_field author_verifying_key)"
POINTER_CODE_HASH="$(top_level_field pointer_code_hash)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "=============================================================="
echo " Pointer record publish — pre-flight"
echo "=============================================================="

# ---------------------------------------------------------------- PROVENANCE
echo ""
echo "[provenance] clean tree, on main, at origin/main, CI green"
if [ -n "$(git status --porcelain)" ]; then
    git status --short | sed 's/^/    /'
    die "working tree is not clean. Publish only from a clean checkout of main."
fi
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || die "on branch '$BRANCH'. Publish only from main (see ~/.claude/rules/publish-from-main.md)."
git fetch origin main --quiet
HEAD_SHA="$(git rev-parse HEAD)"
ORIGIN_SHA="$(git rev-parse origin/main)"
[ "$HEAD_SHA" = "$ORIGIN_SHA" ] || die "HEAD ($HEAD_SHA) != origin/main ($ORIGIN_SHA). Pull first."
say "HEAD == origin/main == $HEAD_SHA"

if command -v gh >/dev/null 2>&1; then
    # Non-SUCCESS conclusions include failure, cancelled and timed_out; a
    # still-running check is also not a green signal.
    BAD="$(gh run list --repo freenet/river --commit "$HEAD_SHA" \
             --json conclusion,status,name \
             --jq '[.[] | select(.status != "completed" or (.conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral"))] | length' 2>/dev/null || echo "?")"
    if [ "$BAD" = "?" ]; then
        say "WARNING: could not read CI status for $HEAD_SHA — verify by hand"
    elif [ "$BAD" != "0" ]; then
        gh run list --repo freenet/river --commit "$HEAD_SHA" --limit 20 | sed 's/^/    /'
        die "$BAD CI check(s) on $HEAD_SHA are not green. Never publish on red or pending CI."
    else
        say "CI green on $HEAD_SHA"
    fi
else
    say "WARNING: gh not found — verify CI on $HEAD_SHA by hand"
fi

# ------------------------------------------------------------------ THE NODE
echo ""
echo "[node] the target node is the intended one, and is on the network"
PEERS="$(fdev -p "$PORT" query 2>/dev/null | grep -cE '^\| [1-9A-HJ-NP-Za-km-z]{17} ' || true)"
[ "${PEERS:-0}" -gt 0 ] || die "node on port $PORT reports no connected peers. A PUT there reaches nobody."
say "node on port $PORT has $PEERS connected peer(s)"

# ------------------------------------------------------- 1. WHICH BYTES ARE NAMED
# The pointer WASM must be the COMMITTED artifact, never a local rebuild. A
# locally rebuilt WASM lands every record at a key nobody else derives —
# invisible to every consumer, and indistinguishable from success here.
echo ""
echo "[1] the pointer WASM we PUT hashes to the published pointer code hash"
[ -n "$POINTER_WASM" ] || die "set --pointer-wasm (or \$POINTER_WASM) to the COMMITTED pointer-v1.wasm.
Do NOT build it: see contracts/pointer-contract/WASM-STABILITY.md upstream."
[ -f "$POINTER_WASM" ] || die "pointer WASM not found: $POINTER_WASM"
ACTUAL_PCH="$(pointer-codehash "$POINTER_WASM" 2>/dev/null || true)"
if [ -z "$ACTUAL_PCH" ]; then
    die "could not compute the pointer WASM's code hash (is pointer-codehash installed?)"
fi
[ "$ACTUAL_PCH" = "$POINTER_CODE_HASH" ] || die "the pointer WASM at $POINTER_WASM hashes to
  $ACTUAL_PCH
but $TOML_PATH names
  $POINTER_CODE_HASH
Publishing this would put every River record at an address nobody derives.
You have almost certainly rebuilt the WASM locally. Use the committed artifact."
say "$POINTER_WASM -> $ACTUAL_PCH (matches)"

# ----------------------------------------- 2. AGAINST WHAT IS ACTUALLY ON THE NETWORK
# The record must name the hash of the artifact users are actually running, not
# the one in this checkout. Those diverge in practice. River ships both WASMs
# INSIDE the web container's webapp archive, so the live bytes are fetchable.
echo ""
echo "[2] the code hash in each record == the artifact LIVE ON THE NETWORK"
say "fetching the live web container ($WEB_CONTAINER_KEY) ..."
fdev -p "$PORT" execute get --timeout 180 -o "$WORK/wc.state" "$WEB_CONTAINER_KEY" >"$WORK/wc.log" 2>&1 \
    || { tail -5 "$WORK/wc.log" | sed 's/^/    /'; die "could not GET the live web container"; }
python3 - "$WORK/wc.state" "$WORK/webapp.tar.xz" <<'PY'
import struct, sys
d = open(sys.argv[1], 'rb').read()
off = 0
mlen = struct.unpack('>Q', d[off:off+8])[0]; off += 8 + mlen
wlen = struct.unpack('>Q', d[off:off+8])[0]; off += 8
open(sys.argv[2], 'wb').write(d[off:off+wlen])
PY
mkdir -p "$WORK/live"
tar -xJf "$WORK/webapp.tar.xz" -C "$WORK/live" contracts/ 2>/dev/null \
    || die "could not unpack contracts/ from the live webapp archive"

# --------------------------------------------------------------- PER-RECORD
N="$(grep -c '^\[\[record\]\]' "$TOML_PATH")"
declare -a PUB_APP PUB_KEY PUB_STATE PUB_VERSION PUB_HASH
for i in $(seq 1 "$N"); do
    APP_ID="$(field_of_record "$i" app_id)"
    WASM_PATH="$(field_of_record "$i" wasm_path)"
    VERSION="$(field_of_record "$i" version)"
    CODE_HASH="$(field_of_record "$i" code_hash)"
    STATE="$(field_of_record "$i" state)"
    POINTER_KEY="$(field_of_record "$i" pointer_key)"

    echo ""
    echo "--- $APP_ID ---"

    LIVE_WASM="$WORK/live/contracts/$(basename "$WASM_PATH")"
    if [ -f "$LIVE_WASM" ]; then
        LIVE_HASH="$(b3sum "$LIVE_WASM" | cut -d' ' -f1)"
        if [ "$LIVE_HASH" != "$CODE_HASH" ]; then
            die "the record names $CODE_HASH
but the artifact LIVE on the network hashes to $LIVE_HASH.

This is the divergence that matters: publishing this record would point every
integrator at code that is not what users are running. Republish the UI first
(cargo make publish-river), or re-sign against the live bytes."
        fi
        say "[2] live network copy matches: $LIVE_HASH"
    else
        die "could not find $(basename "$WASM_PATH") in the live webapp archive.
Refusing to publish a record whose target could not be checked against the network."
    fi

    # 3. app_id / derived key. A typo in app_id produces a perfectly valid
    #    record at an address nobody ever queries.
    DERIVED="$(pointer-record key --author-vk "$AUTHOR_VK" --app-id "$APP_ID" | sed -n 's/^key=//p')"
    [ "$DERIVED" = "$POINTER_KEY" ] || die "[3] derived key $DERIVED != recorded $POINTER_KEY for $APP_ID"
    say "[3] app_id '$APP_ID' derives to $DERIVED (matches)"

    # 5. The signature verifies, under the key published in FREENET.md, BEFORE
    #    the PUT. A key-file/doc mismatch must fail loudly rather than ship a
    #    record integrators cannot verify.
    grep -qF "$AUTHOR_VK" FREENET.md || die "[5] FREENET.md does not publish $AUTHOR_VK"
    pointer-record verify --author-vk "$AUTHOR_VK" --app-id "$APP_ID" --state "$STATE" \
        --expect-version "$VERSION" --expect-code-hash "$CODE_HASH" --expect-key "$POINTER_KEY" >/dev/null \
        || die "[5] the record for $APP_ID does not verify"
    say "[5] signature verifies against the key published in FREENET.md"

    # 4. The version must be exactly one above what is ON THE NETWORK, read
    #    from the network rather than assumed.
    ONNET_VERSION=""
    if fdev -p "$PORT" execute get --timeout 120 -o "$WORK/cur_$i.bin" "$POINTER_KEY" >"$WORK/cur_$i.log" 2>&1 \
       && [ -s "$WORK/cur_$i.bin" ]; then
        ONNET_VERSION="$(pointer-record verify --author-vk "$AUTHOR_VK" --app-id "$APP_ID" \
                          --state "$WORK/cur_$i.bin" | sed -n 's/^version=//p')"
    fi
    if [ -z "$ONNET_VERSION" ]; then
        say "[4] nothing on the network yet — this is the FIRST publish (PUT), version $VERSION"
        PUB_OP="put"
    else
        EXPECT=$((ONNET_VERSION + 1))
        [ "$VERSION" -eq "$EXPECT" ] || die "[4] on-network version is $ONNET_VERSION, so this publish must be
version $EXPECT, but the record says $VERSION.
A publish at an already-used version is a silent NO-OP: the network accepts it
and keeps the OLD record. Re-sign with the right version."
        say "[4] on-network version $ONNET_VERSION -> publishing $VERSION (UPDATE)"
        PUB_OP="update"
    fi

    PUB_APP[$i]="$APP_ID"; PUB_KEY[$i]="$POINTER_KEY"; PUB_STATE[$i]="$STATE"
    PUB_VERSION[$i]="$VERSION"; PUB_HASH[$i]="$CODE_HASH"
    printf '%s' "$STATE" | xxd -r -p > "$WORK/state_$i.bin"
    echo "$PUB_OP" > "$WORK/op_$i"
done

echo ""
echo "=============================================================="
echo " All pre-flight checks PASSED."
echo "=============================================================="
for i in $(seq 1 "$N"); do
    echo "  $(cat "$WORK/op_$i" | tr a-z A-Z) ${PUB_APP[$i]} v${PUB_VERSION[$i]} -> ${PUB_KEY[$i]}"
done

if [ "$DRY_RUN" -eq 1 ]; then
    echo ""
    echo "--dry-run: stopping before the network write."
    exit 0
fi

if [ "$ASSUME_YES" -eq 0 ]; then
    echo ""
    read -r -p "Publish these records to the network? [y/N] " REPLY
    case "$REPLY" in y|Y|yes|YES) ;; *) echo "aborted."; exit 1 ;; esac
fi

# ------------------------------------------------------------------ PUBLISH
for i in $(seq 1 "$N"); do
    echo ""
    echo "--- publishing ${PUB_APP[$i]} ---"
    PARAMS="$WORK/params_$i.bin"
    pointer-record key --author-vk "$AUTHOR_VK" --app-id "${PUB_APP[$i]}" \
        | sed -n 's/^params=//p' | xxd -r -p > "$PARAMS"
    if [ "$(cat "$WORK/op_$i")" = "put" ]; then
        fdev -p "$PORT" execute put --timeout 180 \
            --code "$POINTER_WASM" --parameters "$PARAMS" \
            contract --state "$WORK/state_$i.bin" 2>&1 | tail -3 | sed 's/^/    /'
    else
        fdev -p "$PORT" execute update --timeout 180 \
            "${PUB_KEY[$i]}" "$WORK/state_$i.bin" 2>&1 | tail -3 | sed 's/^/    /'
    fi
done

# --------------------------------------------------------------- VERIFY BACK
# "The PUT returned OK" is not evidence. A stale publish is a no-op SUCCESS, so
# the only check that proves an integrator gets the right answer is to read the
# record back and resolve it end to end.
echo ""
echo "=============================================================="
echo " Post-publish verification (the only check that proves anything)"
echo "=============================================================="
FAILED=0
for i in $(seq 1 "$N"); do
    echo ""
    echo "--- ${PUB_APP[$i]} ---"
    rm -f "$WORK/back_$i.bin"
    if ! fdev -p "$PORT" execute get --timeout 180 -o "$WORK/back_$i.bin" "${PUB_KEY[$i]}" \
         >"$WORK/back_$i.log" 2>&1 || [ ! -s "$WORK/back_$i.bin" ]; then
        echo "  FAILED: could not read the record back from ${PUB_KEY[$i]}"
        FAILED=1
        continue
    fi
    if ! cmp -s "$WORK/back_$i.bin" "$WORK/state_$i.bin"; then
        echo "  FAILED: the bytes on the network are not the bytes we published."
        echo "  This is what a silently-ignored stale publish looks like."
        FAILED=1
        continue
    fi
    say "byte-identical to what we published"
    if ! pointer-record verify --author-vk "$AUTHOR_VK" --app-id "${PUB_APP[$i]}" \
            --state "$WORK/back_$i.bin" \
            --expect-version "${PUB_VERSION[$i]}" \
            --expect-code-hash "${PUB_HASH[$i]}" \
            --expect-key "${PUB_KEY[$i]}" >/dev/null; then
        echo "  FAILED: the record read BACK from the network does not verify"
        FAILED=1
        continue
    fi
    say "verifies from the network at version ${PUB_VERSION[$i]}, code hash ${PUB_HASH[$i]}"
done

echo ""
if [ "$FAILED" -ne 0 ]; then
    echo "PUBLISH DID NOT FULLY VERIFY. Do not announce these pointers."
    exit 1
fi
echo "All $N record(s) published AND verified from the network."
echo ""
echo "An integrator resolving these now gets the code hash of the artifact"
echo "that is actually live. Addressing only — say nothing about data survival."
