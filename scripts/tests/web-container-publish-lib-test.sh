#!/usr/bin/env bash
# Exercise the web-container publish decisions with stubbed inputs.
#
# The publish path cannot be rehearsed end to end — it needs a live network,
# and testing against the shared contract is out of bounds. So the decisions it
# turns on live in scripts/web-container-publish-lib.sh, with no I/O in them,
# and this runs every branch.
#
# Run: ./scripts/tests/web-container-publish-lib-test.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/web-container-publish-lib.sh
. "$SCRIPT_DIR/../web-container-publish-lib.sh"

PASS=0
FAIL=0

check() { # check <label> <expected> <actual>
    if [ "$2" = "$3" ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
        echo "FAIL: $1"
        echo "      expected: $2"
        echo "      actual:   $3"
    fi
}

echo "--- wc_next_version: the network is the floor, the counter is a hint"

# The ordinary case: counter is ahead, use it.
check "counter ahead of network" \
    "30000386" "$(wc_next_version 30000385 known 30000384)"

# THE 2026-08-04 CASE. The counter was rolled back to 30000376 after a publish
# that had in fact landed at 30000377. Old behaviour signed 30000377 again.
# The floor must lift it to 30000378.
check "counter behind the network (the 1032d373 rollback)" \
    "30000378" "$(wc_next_version 30000376 known 30000377)"

# Equal is the same trap wearing a different hat: counter+1 would be exactly
# the live version.
check "counter equal to the network" \
    "30000378" "$(wc_next_version 30000377 known 30000377)"

# Strictly greater, not contiguous: a network far ahead is followed, not
# stepped toward.
check "network far ahead" \
    "40000001" "$(wc_next_version 12 known 40000000)"

check "absent has no floor" "31" "$(wc_next_version 30 absent)"
check "unknown has no floor" "31" "$(wc_next_version 30 unknown)"

check "a bad status is an error, not a version" \
    "1" "$(wc_next_version 30 nonsense 2>/dev/null >/dev/null; echo $?)"

echo "--- wc_preflight_decision: ambiguity refuses"

check "a verified network version proceeds" \
    "proceed" "$(wc_preflight_decision known 0 0)"

# 'Could not determine' is not 'nothing is there'. Refuse by default; the
# override exists because a node outage should not be able to block a release
# outright, only make the operator say so out loud.
case "$(wc_preflight_decision unknown 0 0)" in
    refuse:*) check "unknown refuses by default" "yes" "yes" ;;
    *)        check "unknown refuses by default" "yes" "no"  ;;
esac
check "unknown proceeds under an explicit override" \
    "proceed" "$(wc_preflight_decision unknown 1 0)"

# NotFound is not proof of absence for a contract we know is published: a
# Freenet GET can dead-end and answer NotFound for state that exists.
case "$(wc_preflight_decision absent 0 0)" in
    refuse:*) check "absent refuses by default" "yes" "yes" ;;
    *)        check "absent refuses by default" "yes" "no"  ;;
esac
check "absent proceeds only for a declared first publish" \
    "proceed" "$(wc_preflight_decision absent 0 1)"

# The unverified override must NOT quietly also accept a NotFound: they are
# different claims and one is not a licence for the other.
case "$(wc_preflight_decision absent 1 0)" in
    refuse:*) check "allow-unverified does not imply allow-first-publish" "yes" "yes" ;;
    *)        check "allow-unverified does not imply allow-first-publish" "yes" "no"  ;;
esac

echo "--- wc_publish_outcome: the exit code decides nothing"

# fdev said OK and our bytes are live: published.
check "rc=0, our version, our bytes" \
    "landed" "$(wc_publish_outcome 0 known 30000378 1 30000378)"

# THE 2026-08-04 CASE, from the other end. `put timed out after 1 peer
# attempt(s)` — non-zero exit — but the state was live all along. Retrying
# here is what produced the fork.
check "rc!=0 but our bytes are live (the timeout that landed)" \
    "landed" "$(wc_publish_outcome 1 known 30000377 1 30000377)"

# Our version is live carrying bytes that are not ours. Both are validly
# signed, so only the byte comparison can see this.
check "our version, someone else's bytes" \
    "collision" "$(wc_publish_outcome 0 known 30000377 0 30000377)"

# The node we published through reported success and the network is still
# behind: whatever it accepted, our bytes are not what is live.
check "rc=0 but the network is still behind" \
    "not-landed" "$(wc_publish_outcome 0 known 30000376 0 30000377)"

check "the network moved above us" \
    "superseded" "$(wc_publish_outcome 0 known 30000380 0 30000377)"

# No read-back means no verdict. Never 'landed' by default.
check "read-back failed" \
    "unknown" "$(wc_publish_outcome 0 unknown "" na 30000377)"
check "read-back said not found" \
    "unknown" "$(wc_publish_outcome 0 absent "" na 30000377)"
check "read-back verified but printed no version" \
    "unknown" "$(wc_publish_outcome 0 known "" na 30000377)"

echo "--- wc_readback_is_final: only a settled answer stops asking"

# These three are statements about a state we read. They do not un-happen, and
# re-reading only delays a report the operator needs.
for outcome in landed collision superseded; do
    check "$outcome is final" "yes" "$(wc_readback_is_final "$outcome")"
done

# THE ONE THAT MATTERS. 'not-landed' says "the network is not showing our
# state", which on an eventually-consistent network is exactly what a read
# taken too early says. Calling it final reports a publish that LANDED as NOT
# PUBLISHED, and that report is what invites the retry that forked the site.
check "not-landed is NOT final" "no" "$(wc_readback_is_final not-landed)"
check "unknown is NOT final"    "no" "$(wc_readback_is_final unknown)"

# Anything unrecognised keeps asking rather than settling on a verdict nothing
# produced.
check "an unrecognised outcome is not final" "no" "$(wc_readback_is_final garbage)"

echo "--- wc_outcome_exit_code: only a proven publish exits 0"

check "landed exits 0"      "0" "$(wc_outcome_exit_code landed)"
for outcome in collision superseded not-landed unknown garbage; do
    rc="$(wc_outcome_exit_code "$outcome")"
    if [ "$rc" -ne 0 ]; then
        check "$outcome is non-zero" "yes" "yes"
    else
        check "$outcome is non-zero" "yes" "no"
    fi
done

echo ""
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
