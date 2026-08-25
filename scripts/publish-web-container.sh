#!/usr/bin/env bash
# Sign and publish River's web container, reading the network before signing
# and again after publishing.
#
# Invoked by `cargo make publish-river` / `cargo make publish-river-debug`, and
# in --sign-only form by `cargo make sign-webapp`. Not usually run by hand.
#
# ## Why this script exists
#
# 2026-08-04, River commit 1032d373. `cargo make publish-river` signed web
# container version 30000377 and uploaded it. `fdev network publish` reported
# `put timed out after 1 peer attempt(s)`, so the rollback arm in
# [tasks.publish-river] restored the counter to 30000376 and the operator
# retried. The retry re-ran `sign-webapp`, which REBUILT the UI — producing a
# different archive, because the build is not reproducible — signed it as
# 30000377 again, and published it. Only then did the network say
# `New state version 30000377 must be higher than current version 30000377`:
# the first PUT had landed after all.
#
# Two different archives, both validly signed at version 30000377, both on the
# network.
#
# ## Why that forks the site rather than resolving
#
# The container IS correctly gated: `update_state` rejects
# `version <= current_version`. That makes DIFFERING versions converge. It does
# nothing for two states at the SAME version — it rejects in both directions,
# so every peer keeps whichever it saw first. And `summarize_state` emits only
# the u32 version, so anti-entropy between two split peers compares equal
# summaries and never tries to heal. The split is silent.
#
# The second archive reaches peers at all because a peer NEW to the contract
# takes the initial-state bypass, which runs `validate_state` only (signature,
# and version != 0) and not the update gate.
#
# River self-heals at the next successful publish, since a strictly higher
# version is accepted by both branches, so exposure is bounded at one release
# cycle. Bounded, not harmless: for that window two populations of users are on
# different builds of the site with no signal that they are.
#
# ## The three things that were missing
#
# 1. READ BEFORE SIGNING. The version to sign at is
#    `max(local counter, on-network version) + 1`, where the on-network version
#    is read from the network and verified under our own contract parameters —
#    not assumed from a local file. A version we cannot prove is above the live
#    one does not get signed.
#
# 2. ABSENCE MUST BE PROVEN. A failed GET means "we did not learn", not
#    "nothing is there". This script separates known / absent / unknown and
#    refuses on ambiguity. It goes one step further than
#    scripts/publish-pointer-records.sh, which treats a `Contract not found` as
#    proof of a first publish: for THIS contract that answer is not proof of
#    anything, because a Freenet GET can dead-end and report NotFound for a
#    contract that exists, and the web container is one we know is published.
#
# 3. RE-READ AFTER PUBLISHING. fdev's exit code is not evidence in either
#    direction. A publish at an already-used version is a no-op SUCCESS, and a
#    publish that reports a timeout may have landed anyway — 2026-08-04 was the
#    second case. So the only thing that establishes "our state is live" is
#    fetching it back and comparing the archive byte for byte. Signature
#    validity cannot do it: both forked archives were validly signed.
#
# ## And one thing that was actively harmful
#
# The counter no longer rolls back. Rolling back on a failed publish is what
# handed the retry a version that had, in fact, already been used. The counter
# is now forward-only: gaps are fine (the contract enforces monotonicity, not
# contiguity) and a bumped-but-unpublished counter costs nothing, because the
# read-before-sign floor recomputes from the network anyway. A burned version
# is never reissued.
#
# ## Usage
#
#   scripts/publish-web-container.sh              # sign, publish, verify
#   scripts/publish-web-container.sh --sign-only  # sign only (cargo make sign-webapp)
#   scripts/publish-web-container.sh --dry-run    # pre-flight only, sign nothing
#
# Environment:
#   WS_API_PORT                     node the publish goes THROUGH (fdev's own
#                                   variable; default 7509). The pre-flight read
#                                   and the publish deliberately share it, so
#                                   the version we check is the version the
#                                   publish is measured against.
#   RIVER_WC_ALLOW_UNVERIFIED=1     proceed even though the on-network version
#                                   could not be determined (see below)
#   RIVER_WC_ALLOW_FIRST_PUBLISH=1  accept "Contract not found" as a genuine
#                                   first publish (for a NEW contract only)
#   RIVER_WC_GET_TIMEOUT            seconds, default 180
#   RIVER_WC_PUBLISH_TIMEOUT        seconds, default 300
#   RIVER_WC_READBACK_ATTEMPTS      default 3
#   RIVER_WC_READBACK_DELAY         seconds between read-back attempts, default 15
#   RIVER_WC_KEY_FILE               signing key file (default:
#                                   ~/.config/river/web-container-keys.toml).
#                                   Exists so this script can be rehearsed
#                                   against a throwaway key and a stub node
#                                   without going anywhere near the production
#                                   one; the parameters check below fails
#                                   closed if the key is not this contract's.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Kept because the flock re-exec below has to hand this script its own
# arguments, and by then the parse loop has consumed "$@".
ORIGINAL_ARGS=("$@")

# shellcheck source=scripts/web-container-publish-lib.sh
. "$SCRIPT_DIR/web-container-publish-lib.sh"

die() { echo "" >&2; echo "ERROR: $*" >&2; exit 1; }
say() { echo "  $*"; }

SIGN_ONLY=0
DRY_RUN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --sign-only) SIGN_ONLY=1; shift ;;
        --dry-run)   DRY_RUN=1; shift ;;
        *) die "unknown argument '$1'" ;;
    esac
done

VERSION_FILE="published-contract/contract-version.txt"
PARAMS_FILE="published-contract/webapp.parameters"
WASM_FILE="published-contract/web_container_contract.wasm"
CONTRACT_ID_FILE="published-contract/contract-id.txt"
ARCHIVE="target/webapp/webapp.tar.xz"
METADATA="target/webapp/webapp.metadata"
SIGNED_PARAMS="target/webapp/webapp.parameters"
TOOL="target/native/x86_64-unknown-linux-gnu/${BUILD_PROFILE:-release}/web-container-tool"
LOCK_FILE="published-contract/.publish.lock"

GET_TIMEOUT="${RIVER_WC_GET_TIMEOUT:-180}"
PUBLISH_TIMEOUT="${RIVER_WC_PUBLISH_TIMEOUT:-300}"
READBACK_ATTEMPTS="${RIVER_WC_READBACK_ATTEMPTS:-3}"
READBACK_DELAY="${RIVER_WC_READBACK_DELAY:-15}"
ALLOW_UNVERIFIED="${RIVER_WC_ALLOW_UNVERIFIED:-0}"
ALLOW_FIRST_PUBLISH="${RIVER_WC_ALLOW_FIRST_PUBLISH:-0}"

# --------------------------------------------------------------- SINGLE WRITER
#
# Two concurrent runs would race the read-modify-write on the counter and could
# sign two archives at the same version — the fork, arrived at from a different
# direction. [tasks.sign-webapp] has carried a comment saying "wrap in flock if
# you need that" since the counter was introduced; nothing ever did.
#
# A missing flock WARNS rather than refusing. flock is Linux-only (these tasks
# already build x86_64-unknown-linux-gnu, so that is the expected platform), and
# blocking a release because a lock utility is absent is a worse failure than
# the race it prevents — the race needs two simultaneous publishes by one
# operator on one machine.
if [ -z "${RIVER_WC_PUBLISH_LOCKED:-}" ]; then
    if command -v flock >/dev/null 2>&1; then
        mkdir -p "$(dirname "$LOCK_FILE")"
        : > "$LOCK_FILE" 2>/dev/null || true
        export RIVER_WC_PUBLISH_LOCKED=1
        set +e
        flock --nonblock --conflict-exit-code 75 "$LOCK_FILE" "$0" \
            ${ORIGINAL_ARGS[@]+"${ORIGINAL_ARGS[@]}"}
        rc=$?
        set -e
        if [ "$rc" -eq 75 ]; then
            die "another web-container publish is already running (lock: $LOCK_FILE).
Two concurrent runs race the version counter and can sign two different
archives at the same version. Wait for the other one, or remove the lock file
if you are certain nothing is running."
        fi
        exit "$rc"
    fi
    echo "WARNING: flock not found — running WITHOUT the single-writer lock." >&2
    echo "         Do not run a second publish concurrently." >&2
fi

# ------------------------------------------------------------- PRECONDITIONS
echo "=============================================================="
echo " River web-container publish — pre-flight"
echo "=============================================================="
echo ""

for tool in fdev cmp; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found (needed by this script)"
done
[ -x "$TOOL" ] || die "$TOOL not found. Run via cargo make so it gets built."
for f in "$VERSION_FILE" "$PARAMS_FILE" "$WASM_FILE" "$CONTRACT_ID_FILE"; do
    [ -f "$f" ] || die "$f missing."
done
[ -s "$ARCHIVE" ] || die "$ARCHIVE missing or empty. Run 'cargo make compress-webapp' first."

CONTRACT_ID="$(tr -d '[:space:]' < "$CONTRACT_ID_FILE")"
COUNTER="$(tr -d '[:space:]' < "$VERSION_FILE")"
case "$COUNTER" in
    ''|*[!0-9]*) die "$VERSION_FILE does not hold a number: '$COUNTER'" ;;
esac

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --------------------------------------------- [1] READ THE NETWORK, BEFORE SIGNING
echo "[1] reading the version currently on the network"
say "contract $CONTRACT_ID (through the node at WS_API_PORT=${WS_API_PORT:-7509})"

NET_STATUS="unknown"
NET_VERSION=""
# `read_network_version <state-out> <log-out>` sets NET_STATUS / NET_VERSION.
read_network_version() {
    local out="$1" log="$2" inspect_out
    NET_STATUS="unknown"
    NET_VERSION=""
    rm -f "$out"
    set +e
    fdev network execute get --timeout "$GET_TIMEOUT" -o "$out" "$CONTRACT_ID" >"$log" 2>&1
    local get_rc=$?
    set -e
    if [ "$get_rc" -eq 0 ] && [ -s "$out" ]; then
        set +e
        inspect_out="$("$TOOL" inspect --state "$out" --parameters "$PARAMS_FILE" 2>&1)"
        local inspect_rc=$?
        set -e
        if [ "$inspect_rc" -ne 0 ]; then
            printf '%s\n' "$inspect_out" | sed 's/^/      /'
            die "the network returned bytes for $CONTRACT_ID that do NOT verify under
our own web-container parameters. Refusing to reason about a version read out
of state whose provenance is unclear."
        fi
        NET_VERSION="$(printf '%s\n' "$inspect_out" | sed -n 's/^version=//p')"
        case "$NET_VERSION" in
            ''|*[!0-9]*) die "web-container-tool inspect printed no usable version: '$NET_VERSION'" ;;
        esac
        NET_STATUS="known"
        return 0
    fi
    if grep -qF "Contract not found" "$log"; then
        NET_STATUS="absent"
    else
        NET_STATUS="unknown"
    fi
}

read_network_version "$WORK/onnet.bin" "$WORK/onnet.log"

case "$NET_STATUS" in
    known)   say "the network holds version $NET_VERSION (signature verified under our parameters)" ;;
    absent)  say "the node answered: Contract not found" ;;
    unknown) tail -3 "$WORK/onnet.log" | sed 's/^/      /'; say "could NOT determine the on-network version" ;;
esac

DECISION="$(wc_preflight_decision "$NET_STATUS" "$ALLOW_UNVERIFIED" "$ALLOW_FIRST_PUBLISH")"
if [ "$DECISION" != "proceed" ]; then
    case "$NET_STATUS" in
        absent)
            die "${DECISION#refuse: }

'Contract not found' is proof of absence for a contract that was never
published. It is not proof for this one: a Freenet GET can dead-end and answer
NotFound for a contract that exists, and published-contract/ says this contract
IS live (counter $COUNTER).

Treating that as a first publish would sign at counter+1 with no floor, which is
how a version that is already in use gets reissued — the 2026-08-04 fork.

Retry, or fix the node. If this really is a brand-new contract, set
RIVER_WC_ALLOW_FIRST_PUBLISH=1."
            ;;
        *)
            die "${DECISION#refuse: }

That is 'we did not learn', not 'nothing is there'. Signing at counter+1 here
would be signing at a version we cannot prove is above the live one, which is
exactly what forked the site on 2026-08-04.

Retry, or fix the node. If you must proceed anyway, set
RIVER_WC_ALLOW_UNVERIFIED=1 — the post-publish read-back still runs, and will
tell you if the state did not land."
            ;;
    esac
fi
if [ "$NET_STATUS" != "known" ] && [ "$SIGN_ONLY" -eq 0 ]; then
    echo ""
    echo "  !! proceeding WITHOUT a verified on-network version, by explicit override." >&2
    echo "  !! the read-back after the publish is the only remaining check." >&2
fi

# ------------------------------------------------------------ [2] PICK A VERSION
VERSION="$(wc_next_version "$COUNTER" "$NET_STATUS" "$NET_VERSION")"
echo ""
echo "[2] version to sign"
if [ "$NET_STATUS" = "known" ] && [ "$NET_VERSION" -ge "$COUNTER" ]; then
    say "local counter $COUNTER is not above the network's $NET_VERSION — using the network as the floor"
fi
say "signing version $VERSION (counter $COUNTER, network ${NET_VERSION:-unknown})"
if [ "$NET_STATUS" = "known" ] && [ "$VERSION" -ne $((NET_VERSION + 1)) ]; then
    say "note: skipping from $NET_VERSION to $VERSION (gaps are fine — the contract"
    say "      enforces monotonicity, not contiguity)"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo ""
    echo "--dry-run: stopping before signing. Nothing was written."
    exit 0
fi

# ------------------------------------------------------------------- [3] SIGN
echo ""
echo "[3] signing"
# Forward-only. Written BEFORE signing so a crash between here and the publish
# leaves the counter ahead rather than behind: ahead costs a gap, behind costs
# a reissued version.
echo "$VERSION" > "$VERSION_FILE"
# Remove the rollback snapshot the old flow left behind, if a stale one exists.
rm -f "$VERSION_FILE.prev"

KEY_ARGS=()
if [ -n "${RIVER_WC_KEY_FILE:-}" ]; then
    KEY_ARGS=(--key-file "$RIVER_WC_KEY_FILE")
fi
"$TOOL" sign \
    --input "$ARCHIVE" \
    --output "$METADATA" \
    --parameters "$SIGNED_PARAMS" \
    --version "$VERSION" \
    ${KEY_ARGS[@]+"${KEY_ARGS[@]}"}

# The parameters ARE the contract's identity: the contract ID is derived from
# (wasm, parameters). If the key we just signed with is not the key this
# contract belongs to, the publish would go to a different contract entirely
# and every check after it would be measuring the wrong thing.
cmp -s "$SIGNED_PARAMS" "$PARAMS_FILE" || die "the key that just signed does not match $PARAMS_FILE.
Signing with the wrong key publishes to a DIFFERENT contract ID. Check
~/.config/river/web-container-keys.toml."
say "signed version $VERSION with the key that owns $CONTRACT_ID"

if [ "$SIGN_ONLY" -eq 1 ]; then
    echo ""
    echo "--sign-only: not publishing. Commit $VERSION_FILE only after a successful publish."
    exit 0
fi

# ---------------------------------------------------------------- [4] PUBLISH
echo ""
echo "[4] publishing"
set +e
fdev network publish \
    --code "$WASM_FILE" \
    --parameters "$PARAMS_FILE" \
    --timeout "$PUBLISH_TIMEOUT" \
    contract \
    --webapp-archive "$ARCHIVE" \
    --webapp-metadata "$METADATA" >"$WORK/pub.log" 2>&1
PUB_RC=$?
set -e
tail -5 "$WORK/pub.log" | sed 's/^/    /'
if [ "$PUB_RC" -eq 0 ]; then
    say "fdev reported success (exit 0) — which proves nothing on its own"
else
    say "fdev reported FAILURE (exit $PUB_RC) — which also proves nothing on its own"
fi

# --------------------------------------------------------------- [5] READ BACK
echo ""
echo "[5] reading the state back"
echo "    'the publish returned OK' is not evidence: a publish at an already-used"
echo "    version is a no-op success, and a publish that reports a timeout may"
echo "    have landed. Only the bytes on the network settle it."

BACK_STATUS="unknown"
BACK_VERSION=""
BYTES_MATCH="na"
OUTCOME="unknown"
attempt=1
while [ "$attempt" -le "$READBACK_ATTEMPTS" ]; do
    say "attempt $attempt/$READBACK_ATTEMPTS"
    rm -f "$WORK/back.tar.xz"
    set +e
    fdev network execute get --timeout "$GET_TIMEOUT" -o "$WORK/back.bin" "$CONTRACT_ID" \
        >"$WORK/back.log" 2>&1
    get_rc=$?
    set -e
    BACK_STATUS="unknown"
    BACK_VERSION=""
    BYTES_MATCH="na"
    if [ "$get_rc" -eq 0 ] && [ -s "$WORK/back.bin" ]; then
        set +e
        inspect_out="$("$TOOL" inspect --state "$WORK/back.bin" --parameters "$PARAMS_FILE" \
            --archive-out "$WORK/back.tar.xz" 2>&1)"
        inspect_rc=$?
        set -e
        if [ "$inspect_rc" -eq 0 ]; then
            BACK_VERSION="$(printf '%s\n' "$inspect_out" | sed -n 's/^version=//p')"
            case "$BACK_VERSION" in
                ''|*[!0-9]*) BACK_VERSION="" ;;
            esac
            if [ -n "$BACK_VERSION" ]; then
                BACK_STATUS="known"
                if cmp -s "$WORK/back.tar.xz" "$ARCHIVE"; then
                    BYTES_MATCH=1
                else
                    BYTES_MATCH=0
                fi
            fi
        else
            printf '%s\n' "$inspect_out" | sed 's/^/      /'
        fi
    elif grep -qF "Contract not found" "$WORK/back.log"; then
        BACK_STATUS="absent"
    fi

    OUTCOME="$(wc_publish_outcome "$PUB_RC" "$BACK_STATUS" "$BACK_VERSION" "$BYTES_MATCH" "$VERSION")"
    say "network says: ${BACK_STATUS}${BACK_VERSION:+ version $BACK_VERSION}, archive match: $BYTES_MATCH -> $OUTCOME"
    # Only 'unknown' is worth retrying: a definite answer that differs from
    # ours will not change by asking again, and retrying it just delays a
    # report the operator needs.
    if [ "$OUTCOME" != "unknown" ]; then
        break
    fi
    attempt=$((attempt + 1))
    if [ "$attempt" -le "$READBACK_ATTEMPTS" ]; then
        sleep "$READBACK_DELAY"
    fi
done

# ----------------------------------------------------------------- [6] VERDICT
echo ""
echo "=============================================================="
EXIT_CODE="$(wc_outcome_exit_code "$OUTCOME")"
case "$OUTCOME" in
    landed)
        if [ "$PUB_RC" -ne 0 ]; then
            echo " PUBLISHED — despite fdev exiting $PUB_RC"
            echo "=============================================================="
            echo ""
            echo "  Version $VERSION is live and byte-identical to what we published."
            echo "  fdev's non-zero exit was a REPORTING failure, not a publish failure."
            echo "  This is the 2026-08-04 shape exactly: do NOT retry, and do NOT"
            echo "  roll the counter back. Retrying here is what forked the site."
        else
            echo " PUBLISHED"
            echo "=============================================================="
            echo ""
            echo "  Version $VERSION is live and byte-identical to what we published."
        fi
        echo ""
        echo "  Commit the counter:  git add $VERSION_FILE"
        ;;
    collision)
        echo " FORKED — version $VERSION is live carrying DIFFERENT bytes"
        echo "=============================================================="
        echo ""
        echo "  The network holds a state at OUR version whose archive is not ours."
        echo "  Only our key can sign a state this contract accepts, so this is an"
        echo "  earlier run of this pipeline: two archives now exist at version"
        echo "  $VERSION and they will NOT converge — the update gate rejects in"
        echo "  both directions and the state summary is only the version number."
        echo ""
        echo "  Fix: re-run the publish. The pre-flight will now read $VERSION off"
        echo "  the network and sign at $((VERSION + 1)), which BOTH branches accept."
        ;;
    superseded)
        echo " NOT LIVE — the network is at $BACK_VERSION, above our $VERSION"
        echo "=============================================================="
        echo ""
        echo "  Something published a higher version. Our archive is not what users"
        echo "  are getting. Re-run the publish to sign above $BACK_VERSION, and find"
        echo "  out what else is publishing to this contract."
        ;;
    not-landed)
        echo " NOT PUBLISHED — the network is still at $BACK_VERSION"
        echo "=============================================================="
        echo ""
        echo "  The publish did not land. The counter is left at $VERSION and is NOT"
        echo "  rolled back: $VERSION may have been seen by some peer, and reissuing"
        echo "  a version that was already used is the 2026-08-04 fork. Gaps are"
        echo "  fine — the contract enforces monotonicity, not contiguity."
        echo ""
        echo "  Re-run the publish; it will sign above whatever the network reports."
        ;;
    *)
        echo " UNKNOWN — could not read the state back"
        echo "=============================================================="
        echo ""
        echo "  We do not know whether version $VERSION landed. The counter is left"
        echo "  at $VERSION and is NOT rolled back, precisely because we cannot rule"
        echo "  out that it landed."
        echo ""
        echo "  Re-run the publish once the node answers; the pre-flight will read"
        echo "  the live version and sign above it either way."
        ;;
esac
echo ""
exit "$EXIT_CODE"
