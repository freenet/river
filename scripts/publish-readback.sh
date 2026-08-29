#!/usr/bin/env bash
#
# Advisory post-publish read-back for the River web container.
#
# Fetches the web-container state back and tells the operator what is ACTUALLY
# live: the on-network version, and whether its archive is byte-for-byte the
# one just published.
#
# THIS SCRIPT REPORTS; IT DOES NOT GATE. It always exits 0, whatever it finds
# and whatever goes wrong inside it. A publish that succeeded is not
# un-succeeded by a read that failed, and a check that can block a release is
# a check that gets disabled.
#
# It also derives NO version floor from what it reads. Reading the network
# BEFORE signing and signing at a floor taken from that read looks like a
# stronger design and is not: Freenet is eventually consistent, so a
# stale-but-validly-signed read plus a local counter that is behind yields a
# signature at a version the network has already accepted -- the same fork,
# reached more elaborately.
#
# The incident (2026-08-04, freenet/river#634):
#   1. `fdev network publish` reported `put timed out after 1 peer attempt(s)`.
#   2. The state had in fact landed. The exit code said otherwise.
#   3. The publish path rolled the version counter back, so the operator's
#      retry re-used version 30000377.
#   4. The rebuild was not byte-reproducible, so a DIFFERENT archive was
#      signed at that same version, and both were on the network.
#
# Two states at one version never converge: the contract rejects
# `version <= current` in both directions, and `summarize_state` emits only the
# version, so anti-entropy sees them as equal. Recovery is the next successful
# publish at a higher version -- so the exposure is one release cycle, and no
# data is lost.
#
# Step 3 is fixed at the source: the counter is forward-only now (see
# [tasks.sign-webapp] in Makefile.toml). This script addresses step 2, which is
# what set the retry in motion. "It landed despite the timeout, do not retry"
# is the sentence that breaks the chain.

set -u

if [ "$#" -ne 6 ]; then
    cat >&2 <<'USAGE'
usage: publish-readback.sh <contract_id> <parameters> <expected_archive> \
                          <our_version> <publish_rc> <web_container_tool>
USAGE
    # Still 0: a caller that invokes this wrongly has a broken read-back, not
    # a broken publish.
    exit 0
fi

contract_id="$1"
parameters="$2"
expected_archive="$3"
our_version="$4"
publish_rc="$5"
tool="$6"

# fdev's own default is 300s, far too long to sit on after a publish that has
# already reported its result.
readback_timeout="${RIVER_READBACK_TIMEOUT:-60}"

workdir=""
# shellcheck disable=SC2317  # invoked via the EXIT trap below
cleanup() {
    [ -n "$workdir" ] && rm -rf "$workdir"
    return 0
}
trap cleanup EXIT

# Every exit from here on goes through `done_` so the advisory framing is
# never lost, and so no path can return non-zero.
done_() {
    cat <<'FOOTER'

This read-back is advisory: it reports what the network answered, and does not
decide whether the publish succeeded.
=========================================

FOOTER
    exit 0
}

echo ""
echo "=== post-publish read-back (advisory) ==="

# Validated SEPARATELY, not concatenated: an empty version next to rc 0
# concatenates to "0" and would sail through a joint check, leaving both later
# comparisons to error out and fall through to a LANDED that was never
# measured. A check whose own inputs are broken must report unknown, never
# success.
for field in "$our_version" "$publish_rc"; do
    case "$field" in
        *[!0-9]* | "")
            echo "Skipped: version '$our_version' / exit code '$publish_rc' are"
            echo "not both plain integers, so nothing here can be compared."
            done_
            ;;
    esac
done

if [ ! -x "$tool" ] && ! command -v "$tool" >/dev/null 2>&1; then
    echo "Could not read the network: no web-container-tool at '$tool'."
    done_
fi

if ! command -v fdev >/dev/null 2>&1; then
    echo "Could not read the network: 'fdev' is not on PATH."
    done_
fi

workdir="$(mktemp -d)" || { echo "Could not read the network: mktemp failed."; done_; }
state_file="$workdir/state.bin"
archive_file="$workdir/archive.bin"
log="$workdir/log"

reread_hint="    fdev network execute get $contract_id -o /tmp/river-state.bin"

# The outer timeout(1) is what makes "advisory" true by wall clock as well as
# by exit code. `fdev --timeout` bounds only the `client.recv()` wait; the
# WebSocket connect, the send and the close are unbounded, so a node that
# accepts the TCP handshake but never completes the upgrade blocks here
# forever -- and `|| true` at the call site cannot rescue a hang, only a
# non-zero exit. An operator left at a prompt that never returns, with the
# counter bumped but uncommitted, is one Ctrl-C away from the `git checkout`
# on the counter file that this whole change exists to prevent.

# The timeout is validated here, at its first arithmetic use: an operator-supplied
# RIVER_READBACK_TIMEOUT that is not a number would otherwise abort the whole
# script with a raw bash error and no footer, which contradicts the promise at
# the top of this file that it always exits 0 with a report.
case "$readback_timeout" in
    "" | *[!0-9]* | 0*)
        echo "Ignoring RIVER_READBACK_TIMEOUT='$readback_timeout' (not a positive"
        echo "integer); using 60s."
        readback_timeout=60
        ;;
esac

hard_timeout=$((readback_timeout + 10))
get_cmd=(fdev)
if command -v timeout >/dev/null 2>&1; then
    get_cmd=(timeout -k 5 "$hard_timeout" fdev)
fi
# Without timeout(1) the hang is back. Left unhandled deliberately: the tool
# path this script is called with hard-codes x86_64-unknown-linux-gnu, so a
# host running this without coreutils is not a case that occurs.

# `|| get_rc=$?` and NOT `if ! cmd; then get_rc=$?`. Inside `if !` the status
# read by `$?` is the NEGATED one, so it is always 0 and the 124/137 branch
# below would be dead code.
get_rc=0
"${get_cmd[@]}" network execute get "$contract_id" \
    --timeout "$readback_timeout" -o "$state_file" >"$log" 2>&1 || get_rc=$?

if [ "$get_rc" -ne 0 ]; then
    if [ "$get_rc" -eq 124 ] || [ "$get_rc" -eq 137 ]; then
        echo "Gave up reading the network back after ${hard_timeout}s -- the node"
        echo "did not answer. (The publish itself is unaffected by this.)"
    else
        echo "Could not read the network back (fdev execute get failed):"
        sed 's/^/    /' "$log"
    fi
    echo ""
    echo "That says nothing about whether the publish landed. Re-read with"
    echo "$reread_hint"
    echo "before concluding anything; do NOT republish on this line alone."
    done_
fi

if [ ! -s "$state_file" ]; then
    echo "Could not read the network back: the node returned an empty state."
    done_
fi

# `--parameters` is not optional: it makes the tool check the state's signature
# against the key that owns this contract. Without it the version is just
# whatever the responder chose to say.
net_version=""
if "$tool" inspect --state "$state_file" --parameters "$parameters" \
        --archive-out "$archive_file" >"$workdir/out" 2>"$log"; then
    net_version="$(sed -n 's/^version=//p' "$workdir/out")"
fi
case "$net_version" in
    *[!0-9]* | "") net_version="" ;;
esac

if [ -z "$net_version" ]; then
    echo "Read a state back, but could not make sense of it:"
    sed 's/^/    /' "$log"
    echo ""
    echo "An unverified state is not evidence of anything, so this is being"
    echo "reported as unknown rather than as a version."
    done_
fi

# One explicit three-way branch. LANDED is reached only by proving equality --
# never by falling out of the bottom of a chain of failed comparisons, which
# is the direction that hurts: false reassurance at the exact moment the
# operator is deciding whether to retry.
if [ "$net_version" -gt "$our_version" ]; then
    cat <<EOF
SUPERSEDED. The network is at version $net_version, above the $our_version just
published.

Two causes, and they need different responses:
  * Somebody else published after this run. Nothing to do; the newer state
    wins.
  * Your counter was BEHIND the network, so the state signed at $our_version was
    rejected and YOUR CHANGES ARE NOT LIVE. The contract refuses any version
    at or below what it holds.

If nobody else published, it is the second: set the counter above $net_version
and publish again.
EOF
    done_
elif [ "$net_version" -lt "$our_version" ]; then
    cat <<EOF
NOT SEEN YET. The network answered with version $net_version, below the
$our_version just published.

Freenet is eventually consistent and one GET is a sample, not a verdict, so
this is also what a read taken too early looks like. Wait and re-read:
$reread_hint
Republishing on the strength of a single early read is how the same version
gets signed twice.
EOF
    done_
elif [ "$net_version" -ne "$our_version" ]; then
    # Unreachable while both operands are validated integers. Kept so that a
    # future change which breaks that lands here rather than in LANDED.
    echo "Could not compare version $net_version against $our_version. Reporting"
    echo "this as unknown rather than as a result."
    done_
fi

# `cmp` returns 0 for same, 1 for differ, and >= 2 for "could not compare" --
# an unreadable file among them. Treating >= 2 as "differ" renders a local
# tooling problem as FORKED, the most alarming verdict this script has.
if [ ! -r "$expected_archive" ] || [ ! -s "$expected_archive" ]; then
    cat <<EOF
The network is at version $net_version, our version -- but the archive we
published is missing or empty locally ($expected_archive), so there is nothing
to compare it against. Reporting this as unknown: it is a local problem, and
says nothing about what the network holds.
EOF
    done_
fi

cmp -s "$archive_file" "$expected_archive"
cmp_rc=$?

if [ "$cmp_rc" -ge 2 ]; then
    echo "The network is at version $net_version, our version -- but the two"
    echo "archives could not be compared (cmp exited $cmp_rc). Reporting this as"
    echo "unknown rather than guessing which way it went."
    done_
fi

if [ "$cmp_rc" -eq 1 ]; then
    cat <<EOF
FORKED. The network is serving version $net_version -- our version -- but its
archive is NOT the one just published.

Two different states now share one version, and they cannot converge: the
contract rejects version <= current in both directions, and the state summary
carries only the version, so anti-entropy sees them as equal.

Recovery is a fresh publish at a HIGHER version. Bump the counter and publish
again; do not try to republish at $net_version.
EOF
    done_
fi

echo "LANDED. The network is serving version $net_version with the exact archive"
echo "just published."
if [ "$publish_rc" -ne 0 ]; then
    cat <<EOF

Note that fdev exited $publish_rc. It landed anyway -- a timeout is not proof of
failure. Do NOT republish: commit the bumped version counter and stop here.
Republishing after a spurious failure is what forked the site on 2026-08-04.
EOF
fi
done_
