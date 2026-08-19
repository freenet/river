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
# Initialised here, not only where it is computed in the provenance section,
# because it is read again in the Result summary AFTER the network write. Under
# `set -u` an unset read there would kill the script at the one point where
# dying is most expensive: records are already live and the summary saying
# which ones is what the operator has left. The assignment below always runs
# today, so this costs nothing; it stops a future edit that guards or moves
# that assignment from turning a note into a post-publish abort.
UNTRACKED=""
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

# Every external tool this script calls, checked up front. A `command not
# found` in the middle of the publish sequence is the worst place to discover
# one, because some records may already be on the network by then.
for tool in fdev b3sum xxd tar python3 jq gh pointer-record pointer-codehash; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found (needed by this script)"
done
[ -f "$TOML_PATH" ] || die "$TOML_PATH not found"

# The same shared reader the gate and the signer use. All three MUST agree
# about what a record says — see scripts/pointer-toml-lib.sh.
. "$(dirname "$0")/pointer-toml-lib.sh"

field_of_record() { pointer_field "$TOML_PATH" "$1" "$2"; }
top_level_field() { pointer_top_field "$TOML_PATH" "$1"; }

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
# TRACKED modifications only. An untracked file cannot change what is
# published: every path this script reads is tracked, and the records are
# separately checked against the committed blob, the derived key and the
# artifact live on the network. Refusing on untracked files blocked every
# publish in the Atlas port (freenet/atlas#47), whose checkout normally carries
# build artifacts; River's happened to be clean, which is the only reason this
# copy has not hit it.
#
# They are still PRINTED rather than ignored silently — if one of them is a
# surprise, the operator should see it. An interactive run sees it here, before
# the confirmation prompt. An unattended run (--yes) has nobody watching stdout
# at this point, so the same list is REPEATED in the Result summary at the end:
# that is the part of the output a later reader actually reads, and it is what
# makes this note a durable record rather than a line that scrolled past. It
# stays a note either way — untracked files never block a publish.
#
# A modified TRACKED file remains a hard refusal, deliberately: it means this
# checkout differs from the commit whose CI was verified above, which is exactly
# the provenance claim being made.
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    git status --short --untracked-files=no | sed 's/^/    /'
    die "tracked files are modified. Publish only from a clean checkout of main."
fi
UNTRACKED="$(git status --porcelain | grep '^??' || true)"
if [ -n "$UNTRACKED" ]; then
    say "note: untracked files present (they cannot affect what is published):"
    printf '%s\n' "$UNTRACKED" | sed 's/^/      /'
fi
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || die "on branch '$BRANCH'. Publish only from main (see ~/.claude/rules/publish-from-main.md)."
git fetch origin main --quiet
HEAD_SHA="$(git rev-parse HEAD)"
ORIGIN_SHA="$(git rev-parse origin/main)"
[ "$HEAD_SHA" = "$ORIGIN_SHA" ] || die "HEAD ($HEAD_SHA) != origin/main ($ORIGIN_SHA). Pull first."
say "HEAD == origin/main == $HEAD_SHA"

command -v gh >/dev/null 2>&1 || die "gh not found — cannot verify CI on $HEAD_SHA"
RUNS="$(gh run list --repo freenet/river --commit "$HEAD_SHA" --json conclusion,status,name 2>/dev/null || echo "")"
[ -n "$RUNS" ] || die "could not read CI status for $HEAD_SHA. Refusing to publish on an unknown signal."

# COUNT THE RUNS, not only the bad ones. An empty list yields zero failures,
# so a `length == 0` test on its own reports "green" for a commit CI never ran
# on — a guard that cannot fail, which is worse than no guard because it is
# reassuring. Ask for evidence of success, then for absence of failure.
TOTAL="$(printf '%s' "$RUNS" | jq 'length')"
[ "$TOTAL" -gt 0 ] || die "NO CI runs exist for $HEAD_SHA.
That is not the same as green. Either CI has not started, or this commit is not
what you think it is. Refusing to publish."

# Non-SUCCESS conclusions include failure, cancelled and timed_out; a
# still-running check is also not a green signal.
BAD="$(printf '%s' "$RUNS" | jq '[.[] | select(.status != "completed" or (.conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral"))] | length')"
if [ "$BAD" != "0" ]; then
    gh run list --repo freenet/river --commit "$HEAD_SHA" --limit 20 | sed 's/^/    /'
    die "$BAD of $TOTAL CI check(s) on $HEAD_SHA are not green. Never publish on red or pending CI."
fi
say "CI green on $HEAD_SHA ($TOTAL run(s), all successful)"

# ------------------------------------------------------------------ THE NODE
echo ""
echo "[node] the target node is the intended one, and is on the network"
# 16 OR 17 characters. A peer id is base58 of 12 bytes, and base58 of 12 random
# bytes is 16 characters about 18% of the time (measured: 71 of 400). A `{17}`
# pattern therefore undercounts always, and returns ZERO for a node whose peers
# all happen to be short — which would abort every publish claiming the node has
# no peers. Requiring the following column separator keeps the 46/47-character
# contract keys in the same output from matching.
PEERS="$(fdev -p "$PORT" query 2>/dev/null | grep -cE '^\| [1-9A-HJ-NP-Za-km-z]{16,17} +\|' || true)"
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
# No PUB_STATE array: the bytes live in $WORK/state_$i.bin, written below.
# Keeping a second copy in a shell variable would be two things that can
# disagree about what we are publishing.
declare -a PUB_APP PUB_KEY PUB_VERSION PUB_HASH
for i in $(seq 1 "$N"); do
    APP_ID="$(field_of_record "$i" app_id)"
    WASM_PATH="$(field_of_record "$i" wasm_path)"
    VERSION="$(field_of_record "$i" version)"
    CODE_HASH="$(field_of_record "$i" code_hash)"
    STATE="$(field_of_record "$i" state)"
    POINTER_KEY="$(field_of_record "$i" pointer_key)"

    echo ""
    echo "--- $APP_ID ---"

    # Defence in depth: this script should only ever run against a main HEAD
    # that already passed check-pointer-freshness in CI, but it can also be run
    # by hand against a locally-edited file, and an empty field would otherwise
    # reach fdev as an empty argument.
    for v in APP_ID WASM_PATH VERSION CODE_HASH STATE POINTER_KEY; do
        [ -n "${!v}" ] || die "record $i is missing $(echo "$v" | tr 'A-Z_' 'a-z-')"
    done

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

    # 3b. And the NODE agrees. Both values above come from this crate; if the
    # crate and the node ever disagreed about how a key is derived, every check
    # here would pass while the record landed somewhere nobody queries. So ask
    # a second, independent implementation — fdev, from the raw wasm and params.
    # This is the cross-check the pre-publish gate ran as its step 0, and it is
    # the only one that involves an implementation we did not write.
    printf '%s' "$(pointer-record key --author-vk "$AUTHOR_VK" --app-id "$APP_ID" \
        | sed -n 's/^params=//p')" | xxd -r -p > "$WORK/pcheck_$i.bin"
    FDEV_KEY="$(fdev get-contract-id --code "$POINTER_WASM" --parameters "$WORK/pcheck_$i.bin" 2>/dev/null | tail -1)"
    [ "$FDEV_KEY" = "$POINTER_KEY" ] || die "[3b] DERIVATION FORK for $APP_ID.
  this crate derives : $POINTER_KEY
  fdev derives       : $FDEV_KEY
The node and the signing tool disagree about where this record lives. Publishing
would put it at an address no consumer derives, and every other check here would
still pass."
    say "[3b] fdev derives the same key from the raw wasm + params (no convention fork)"

    # 5. The signature verifies, under the key published in FREENET.md, BEFORE
    #    the PUT. A key-file/doc mismatch must fail loudly rather than ship a
    #    record integrators cannot verify.
    grep -qF "$AUTHOR_VK" FREENET.md || die "[5] FREENET.md does not publish $AUTHOR_VK"
    pointer-record verify --author-vk "$AUTHOR_VK" --app-id "$APP_ID" --state "$STATE" \
        --expect-version "$VERSION" --expect-code-hash "$CODE_HASH" --expect-key "$POINTER_KEY" >/dev/null \
        || die "[5] the record for $APP_ID does not verify"
    say "[5] signature verifies against the key published in FREENET.md"

    # The bytes we intend to publish, needed by the comparison below.
    printf '%s' "$STATE" | xxd -r -p > "$WORK/state_$i.bin"

    # 4. The version must be ABOVE what is ON THE NETWORK, read from the
    #    network rather than assumed.
    #
    # ABSENCE MUST BE PROVEN, NOT INFERRED FROM A FAILED GET. A timeout and a
    # genuine "nothing there" both leave us without bytes, and treating them
    # alike takes the PUT branch — which SKIPS the version-monotonicity check
    # below. So a transient hiccup against an already-published pointer would
    # silently downgrade the one check that stops a no-op republish.
    #
    # fdev distinguishes them: a real negative prints
    # `Error: Contract not found: <key>`. Anything else is "we did not learn",
    # and this refuses rather than guessing.
    ONNET_VERSION=""
    GET_OK=0
    if fdev -p "$PORT" execute get --timeout 120 -o "$WORK/cur_$i.bin" "$POINTER_KEY" >"$WORK/cur_$i.log" 2>&1 \
       && [ -s "$WORK/cur_$i.bin" ]; then
        GET_OK=1
        ONNET_VERSION="$(pointer-record verify --author-vk "$AUTHOR_VK" --app-id "$APP_ID" \
                          --state "$WORK/cur_$i.bin" | sed -n 's/^version=//p')"
    fi
    if [ "$GET_OK" -eq 0 ]; then
        if grep -qF "Contract not found: $POINTER_KEY" "$WORK/cur_$i.log"; then
            say "[4] the network reports NOT FOUND — this is the FIRST publish (PUT), version $VERSION"
            PUB_OP="put"
        else
            tail -3 "$WORK/cur_$i.log" | sed 's/^/      /'
            die "[4] could not read $APP_ID's pointer, and the node did NOT say 'not found'.

That is 'we learned nothing', not 'nothing is there'. Publishing as a FIRST
publish here would skip the version check that stops a silent no-op republish
against a pointer that already exists.

Retry, or fix the node. Do not proceed on an ambiguous answer."
        fi
    elif [ -z "$ONNET_VERSION" ]; then
        die "[4] $APP_ID's pointer returned bytes that do not verify under our own author key.
Refusing to publish over state whose provenance is unclear."
    elif cmp -s "$WORK/cur_$i.bin" "$WORK/state_$i.bin"; then
        # Already exactly what we were going to publish. This is the normal
        # state when re-running after a PARTIAL publish, which the failure
        # path explicitly tells the operator to do — so it must not be an
        # error, or the documented recovery would abort on the record that
        # already succeeded.
        say "[4] already on the network at version $ONNET_VERSION, byte-identical — nothing to do"
        PUB_OP="skip"
    else
        # STRICTLY GREATER, not exactly one more. The contract's rule is
        # monotonicity, not contiguity, and an exact `+1` breaks two ordinary
        # situations: re-running after a partial publish, and two WASM changes
        # merging before anyone runs the network publish (local v3, network v1).
        # Both are legitimate; only going backwards or sideways is not.
        [ "$VERSION" -gt "$ONNET_VERSION" ] || die "[4] the network holds version $ONNET_VERSION and this record says $VERSION.
A publish at an already-used or older version is a silent NO-OP: the network
accepts it and keeps the OLD record, and nothing downstream will tell you.
Re-sign at a version above $ONNET_VERSION."
        if [ "$VERSION" -ne $((ONNET_VERSION + 1)) ]; then
            say "[4] note: skipping from $ONNET_VERSION to $VERSION (gaps are fine — the contract"
            say "    enforces monotonicity, not contiguity)"
        fi
        say "[4] on-network version $ONNET_VERSION -> publishing $VERSION (UPDATE)"
        PUB_OP="update"
    fi

    PUB_APP[i]="$APP_ID"; PUB_KEY[i]="$POINTER_KEY"
    PUB_VERSION[i]="$VERSION"; PUB_HASH[i]="$CODE_HASH"
    echo "$PUB_OP" > "$WORK/op_$i"
done

echo ""
echo "=============================================================="
echo " All pre-flight checks PASSED."
echo "=============================================================="
for i in $(seq 1 "$N"); do
    echo "  $(tr '[:lower:]' '[:upper:]' < "$WORK/op_$i") ${PUB_APP[$i]} v${PUB_VERSION[$i]} -> ${PUB_KEY[$i]}"
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

# ------------------------------------------------------- PUBLISH, THEN VERIFY
#
# ONE loop, publishing and verifying each record before moving on.
#
# These used to be two loops. Under `set -e` a failed `fdev` on record 2 killed
# the script before the verification loop ran at all — including for record 1,
# which had already gone live. The header above argues that re-reading is the
# only thing that proves a publish worked; running it in a second loop meant it
# was skipped in exactly the partial-failure case where it matters most, and an
# operator was left inferring what had landed from scrollback.
#
# `errexit` is suspended around each record so one failure cannot take the
# summary down with it.
PUBLISHED=""
FAILED_PUB=""
FAILED_VERIFY=""

verify_one() {
    local i="$1"
    rm -f "$WORK/back_$i.bin"
    if ! fdev -p "$PORT" execute get --timeout 180 -o "$WORK/back_$i.bin" "${PUB_KEY[$i]}" \
         >"$WORK/back_$i.log" 2>&1 || [ ! -s "$WORK/back_$i.bin" ]; then
        echo "  FAILED: could not read the record back from ${PUB_KEY[$i]}"
        return 1
    fi
    if ! cmp -s "$WORK/back_$i.bin" "$WORK/state_$i.bin"; then
        echo "  FAILED: the bytes on the network are not the bytes we published."
        echo "  This is what a silently-ignored stale publish looks like."
        return 1
    fi
    say "byte-identical to what we published"
    if ! pointer-record verify --author-vk "$AUTHOR_VK" --app-id "${PUB_APP[$i]}" \
            --state "$WORK/back_$i.bin" \
            --expect-version "${PUB_VERSION[$i]}" \
            --expect-code-hash "${PUB_HASH[$i]}" \
            --expect-key "${PUB_KEY[$i]}" >/dev/null; then
        echo "  FAILED: the record read BACK from the network does not verify"
        return 1
    fi
    say "verifies from the network at version ${PUB_VERSION[$i]}, code hash ${PUB_HASH[$i]}"
    return 0
}

for i in $(seq 1 "$N"); do
    echo ""
    echo "--- ${PUB_APP[$i]} ---"
    if [ "$(cat "$WORK/op_$i")" = "skip" ]; then
        say "already on the network and byte-identical; verifying only"
        set +e
        verify_one "$i"
        V_RC=$?
        set -e
        if [ "$V_RC" -eq 0 ]; then PUBLISHED="$PUBLISHED ${PUB_APP[$i]}"
        else FAILED_VERIFY="$FAILED_VERIFY ${PUB_APP[$i]}"; fi
        continue
    fi

    # Inside the same errexit suspension as everything else in this loop. It is
    # a local computation that already succeeded during preflight, so a failure
    # here would be surprising — but an unguarded `set -e` abort at this point
    # would kill the script before the Result summary prints, losing the record
    # of which records DID publish. Consistency is cheaper than that risk.
    PARAMS="$WORK/params_$i.bin"
    set +e
    pointer-record key --author-vk "$AUTHOR_VK" --app-id "${PUB_APP[$i]}" \
        | sed -n 's/^params=//p' | xxd -r -p > "$PARAMS"
    PARAMS_RC=$?
    set -e
    if [ "$PARAMS_RC" -ne 0 ] || [ ! -s "$PARAMS" ]; then
        echo "  FAILED: could not derive params for ${PUB_APP[$i]}"
        FAILED_PUB="$FAILED_PUB ${PUB_APP[$i]}"
        continue
    fi

    set +e
    if [ "$(cat "$WORK/op_$i")" = "put" ]; then
        fdev -p "$PORT" execute put --timeout 180 \
            --code "$POINTER_WASM" --parameters "$PARAMS" \
            contract --state "$WORK/state_$i.bin" >"$WORK/pub_$i.log" 2>&1
    else
        fdev -p "$PORT" execute update --timeout 180 \
            "${PUB_KEY[$i]}" "$WORK/state_$i.bin" >"$WORK/pub_$i.log" 2>&1
    fi
    PUB_RC=$?
    set -e
    tail -3 "$WORK/pub_$i.log" | sed 's/^/    /'

    if [ "$PUB_RC" -ne 0 ]; then
        echo "  FAILED: the $(cat "$WORK/op_$i") itself returned $PUB_RC"
        FAILED_PUB="$FAILED_PUB ${PUB_APP[$i]}"
        continue
    fi

    # Verify THIS record now, not after every publish. "The PUT returned OK" is
    # not evidence: a publish at an already-used version is a no-op SUCCESS.
    set +e
    verify_one "$i"
    V_RC=$?
    set -e
    if [ "$V_RC" -eq 0 ]; then
        PUBLISHED="$PUBLISHED ${PUB_APP[$i]}"
    else
        FAILED_VERIFY="$FAILED_VERIFY ${PUB_APP[$i]}"
    fi
done

echo ""
echo "=============================================================="
echo " Result"
echo "=============================================================="
[ -n "$PUBLISHED" ]     && { echo " published AND verified:"; for a in $PUBLISHED; do echo "   $a"; done; }
[ -n "$FAILED_PUB" ]    && { echo " NOT published (the write failed):"; for a in $FAILED_PUB; do echo "   $a"; done; }
[ -n "$FAILED_VERIFY" ] && { echo " WROTE but did NOT verify — treat as UNKNOWN state on the network:"; for a in $FAILED_VERIFY; do echo "   $a"; done; }

# Repeated from the provenance section on purpose. Under --yes nobody is
# watching stdout when that first note prints, so this is the copy an operator
# (or an incident review) actually reads. `if` rather than the `&&` form above
# because this is the last statement before the exit-1 branch, and a failing
# `[` there is exactly where `set -e` semantics get argued about. Not a gate:
# an untracked file cannot change what was published, and the run has already
# finished by the time this prints.
if [ -n "$UNTRACKED" ]; then
    echo " untracked files present at publish time (they cannot affect what was published):"
    printf '%s\n' "$UNTRACKED" | sed 's/^/   /'
fi

if [ -n "$FAILED_PUB" ] || [ -n "$FAILED_VERIFY" ]; then
    echo ""
    echo "PARTIAL PUBLISH. Some records may be live and some not — the lists above"
    echo "say which. Do not announce these pointers. Re-run once the cause is fixed:"
    echo "the version check reads the network, so a record that DID land will be"
    echo "detected and its version requirement recomputed."
    exit 1
fi

echo ""
echo "All $N record(s) published AND verified from the network."
echo ""
echo "An integrator resolving these now gets the code hash of the artifact"
echo "that is actually live. Addressing only — say nothing about data survival."
