#!/usr/bin/env bash
# Pure decision logic for the web-container publish path.
#
# Sourced by scripts/publish-web-container.sh (which does the network I/O) and
# by scripts/tests/web-container-publish-lib-test.sh (which exercises every
# branch below with stubbed inputs). Nothing in this file touches the network,
# the filesystem, or the clock: that is the point — the decisions a publish
# turns on are the part that has to be testable without a network.
#
# See the header of scripts/publish-web-container.sh for the incident these
# rules come from.

# ---------------------------------------------------------------------------
# Vocabulary
#
# A network read of the web container's state lands in exactly one of three
# states, and conflating any two of them is how the 2026-08-04 fork happened:
#
#   known   we fetched a state and its signature verifies under OUR contract
#           parameters, so its version number is a fact.
#   absent  the node answered "Contract not found".
#   unknown we did not learn. A timeout, a dead node, a malformed answer.
#
# "unknown" is NOT "absent", and — for this contract — neither is "absent".
# See wc_preflight_decision.
# ---------------------------------------------------------------------------

# wc_next_version <counter> <net_status> <net_version>
#
# The version to sign at. The local counter is a convenience; the network is
# the authority, so the floor is whichever is higher. That is what makes the
# counter self-healing: a counter that fell behind (a rollback, a discarded
# working-tree change, a fresh clone) can no longer re-issue a version the
# network has already seen.
#
# Strictly greater, never "exactly one more". The contract enforces
# monotonicity, not contiguity, so gaps are fine and always have been.
wc_next_version() {
    local counter="$1" status="$2" net="${3:-}"
    case "$status" in
        known)
            [ -n "$net" ] || { echo "wc_next_version: known status needs a version" >&2; return 1; }
            if [ "$net" -ge "$counter" ]; then
                echo $((net + 1))
            else
                echo $((counter + 1))
            fi
            ;;
        absent|unknown)
            # No floor to raise to. The caller has already decided (via
            # wc_preflight_decision) whether proceeding at all is allowed.
            echo $((counter + 1))
            ;;
        *)
            echo "wc_next_version: unknown status '$status'" >&2
            return 1
            ;;
    esac
}

# wc_preflight_decision <net_status> <allow_unverified> <allow_first_publish>
#
# Prints "proceed" or "refuse: <reason>".
#
# The judgement call this encodes: refusing when the network cannot be read
# blocks a release that might have been perfectly fine, and that is annoying
# but LOUD and self-limiting — the operator retries, or overrides. Signing
# blind is silent and lasts a release cycle, because two states at the same
# version never converge and anti-entropy cannot see the difference (the
# container's summary is just the u32). Loud-and-recoverable beats
# silent-and-forked, so ambiguity refuses by default and both escapes are
# explicit.
wc_preflight_decision() {
    local status="$1" allow_unverified="$2" allow_first_publish="$3"
    case "$status" in
        known)
            echo "proceed"
            ;;
        absent)
            # "Contract not found" is proof of absence for a contract that has
            # never been published. It is NOT proof for this one. A Freenet GET
            # can dead-end and answer NotFound for a contract that exists —
            # that is a tracked, measured failure mode, not a hypothetical — so
            # for an artifact we know is live (published-contract/ is committed
            # and its version counter is non-zero) a NotFound means the request
            # failed to find it, not that it is gone.
            if [ "$allow_first_publish" = "1" ]; then
                echo "proceed"
            else
                echo "refuse: the node answered 'Contract not found' for a contract we know is published."
            fi
            ;;
        unknown)
            if [ "$allow_unverified" = "1" ]; then
                echo "proceed"
            else
                echo "refuse: could not determine the version on the network."
            fi
            ;;
        *)
            echo "refuse: unrecognised network status '$status'"
            ;;
    esac
}

# wc_publish_outcome <publish_rc> <back_status> <back_version> <bytes_match> <our_version>
#
# What actually happened, judged from a read-back rather than from fdev's exit
# code. `bytes_match` is 1/0/na (na when there are no bytes to compare).
#
# The exit code is not evidence in EITHER direction:
#
#   - A zero exit says the node we published THROUGH accepted the PUT. It does
#     not say our bytes are what the network serves.
#   - A publish that reports a timeout may still have landed. That is exactly
#     what happened on 2026-08-04: `put timed out after 1 peer attempt(s)`,
#     and the state was on the network the whole time.
#
# Do NOT restate the old "a publish at an already-used version is a no-op
# SUCCESS" here. It is false for this contract: web-container-contract's
# `update_state` returns `InvalidUpdateWithInfo` for `version <= current`, and
# freenet-core maps a failed PutResponse to an error on the originating node —
# which is how the 2026-08-04 operator saw the rejection message at all. The
# POINTER contract is the one that accepts a stale update silently, by design.
#
# Prints one of: landed | collision | superseded | not-landed | unknown
wc_publish_outcome() {
    local publish_rc="$1" status="$2" version="${3:-}" bytes_match="$4" ours="$5"
    case "$status" in
        known)
            [ -n "$version" ] || { echo "unknown"; return 0; }
            if [ "$version" -eq "$ours" ]; then
                if [ "$bytes_match" = "1" ]; then
                    # Our bytes, our version, live. True regardless of
                    # publish_rc — see the timeout case above.
                    echo "landed"
                else
                    # Our version is live carrying somebody else's bytes.
                    # Only our key can sign a state the contract accepts, so
                    # "somebody else" means an earlier run of this pipeline:
                    # this is the fork.
                    echo "collision"
                fi
            elif [ "$version" -gt "$ours" ]; then
                echo "superseded"
            else
                echo "not-landed"
            fi
            ;;
        absent|unknown)
            echo "unknown"
            ;;
        *)
            echo "unknown"
            ;;
    esac
    # publish_rc is deliberately unused in the classification. It is reported
    # to the operator, but it decides nothing.
    : "$publish_rc"
}

# wc_readback_is_final <outcome>
#
# Prints "yes" when the read-back has settled the question and asking the
# network again cannot change the answer; "no" when it has not.
#
# 'landed', 'collision' and 'superseded' are statements about a state we
# actually read back — the network is at our version with our bytes, at our
# version with bytes that are not ours, or above us. None of those un-happen,
# so re-reading only delays a report the operator needs.
#
# 'not-landed' and 'unknown' are the same claim wearing two hats: "we have not
# seen our state yet". Freenet is eventually consistent, so that is exactly
# what a read taken too early looks like, and one GET is a sample rather than
# a verdict. Treating 'not-landed' as final is how a publish that LANDED gets
# reported as NOT PUBLISHED — and that report is what invites the retry that
# forked the site on 2026-08-04, so it is the direction that hurts.
wc_readback_is_final() {
    case "$1" in
        landed|collision|superseded) echo "yes" ;;
        *)                           echo "no" ;;
    esac
}

# wc_outcome_exit_code <outcome>
#
# 0 only when our state is provably the live one.
wc_outcome_exit_code() {
    case "$1" in
        landed)     echo 0 ;;
        collision)  echo 3 ;;
        superseded) echo 4 ;;
        not-landed) echo 5 ;;
        unknown)    echo 6 ;;
        *)          echo 1 ;;
    esac
}
