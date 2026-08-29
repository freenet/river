#!/usr/bin/env bash
#
# Tests for the two publish guards added after the 2026-08-04 web-container
# fork (freenet/river#634):
#
#   1. the version counter is FORWARD-ONLY -- no publish path rolls it back;
#   2. the post-publish read-back classifies what the network answered, and
#      never fails the publish while doing it.
#
# Run: ./scripts/tests/publish-guards-test.sh

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readback="$repo_root/scripts/publish-readback.sh"
makefile="$repo_root/Makefile.toml"

failures=0
checks=0

ok() {
    checks=$((checks + 1))
    printf 'ok   %s\n' "$1"
}

fail() {
    checks=$((checks + 1))
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    shift
    for line in "$@"; do printf '       %s\n' "$line"; done
}

# ---------------------------------------------------------------------------
# 1. The counter is forward-only.
#
# A source scrape, because the thing being asserted is an ABSENCE and the code
# lives in cargo-make script blocks that cannot be invoked without a network,
# a signing key and a built UI. The rollback that caused the fork was six
# lines of shell in four near-identical copies, and the realistic regression
# is someone reinstating one of them (or adding a fifth publish task that
# copies the old pattern) -- which is exactly what this catches.
# ---------------------------------------------------------------------------

if [[ ! -f "$makefile" ]]; then
    fail "Makefile.toml is readable" "not found at $makefile"
else
    offenders="$(grep -n 'prev_snapshot\|contract-version\.txt\.prev' "$makefile" || true)"
    if [[ -n "$offenders" ]]; then
        mapfile -t offender_lines <<<"$offenders"
        fail "no publish task snapshots the version counter" "${offender_lines[@]}"
    else
        ok "no publish task snapshots the version counter"
    fi

    # The rollback itself: any `mv`/`cp` of something back over the counter
    # file. Comment lines are stripped first -- the whole point of the change
    # is that the Makefile now EXPLAINS at length why it does not roll back,
    # so matching comment text would make the fix fail its own test.
    rollbacks="$(grep -n 'version_file' "$makefile" |
        grep -v ':[[:space:]]*#' |
        grep -E '\b(mv|cp)\b' || true)"
    if [[ -n "$rollbacks" ]]; then
        mapfile -t rollback_lines <<<"$rollbacks"
        fail "no publish task rolls the version counter back" "${rollback_lines[@]}"
    else
        ok "no publish task rolls the version counter back"
    fi

    # ---------------------------------------------------------------------
    # The advisory invariant, at the call site.
    #
    # This is the load-bearing property of the whole design: the read-back
    # must not be able to fail a publish. Dropping `|| true` from an
    # invocation converts it into a publish gate -- the one thing the design
    # forbids -- and nothing else in this suite notices, because the script's
    # own exit-0 discipline is what the other checks exercise.
    #
    # Walks each invocation across its backslash continuations and requires
    # the last line to end in `|| true`. The count is pinned too, so deleting
    # an invocation is caught by the same scrape.
    # ---------------------------------------------------------------------
    unguarded="$(awk '
      /\.\/scripts\/publish-readback\.sh/ { inv = 1; line = $0; start = NR }
      inv {
        line = $0
        if ($0 !~ /\\[[:space:]]*$/) {
          total++
          if (line !~ /\|\|[[:space:]]*true[[:space:]]*$/)
            printf "%d: invocation not terminated by `|| true`\n", start
          inv = 0
        }
      }
      END { printf "count=%d\n", total }
    ' "$makefile")"

    invocation_count="$(sed -n 's/^count=//p' <<<"$unguarded")"
    unguarded="$(grep -v '^count=' <<<"$unguarded" || true)"

    if [[ -n "$unguarded" ]]; then
        mapfile -t unguarded_lines <<<"$unguarded"
        fail "every read-back invocation is guarded by || true" "${unguarded_lines[@]}"
    else
        ok "every read-back invocation is guarded by || true"
    fi

    # `^fdev network publish` at column 0 is the invocation; the same words
    # appear in comments and in the failure-path echo, which are not call
    # sites.
    publish_count="$(grep -c '^fdev network publish' "$makefile" || true)"
    if [[ "$invocation_count" == "$publish_count" && "$invocation_count" -eq 3 ]]; then
        ok "every publish task calls the read-back ($invocation_count of $publish_count)"
    else
        fail "every publish task calls the read-back" \
            "read-back invocations: $invocation_count" \
            "fdev network publish sites: $publish_count" \
            "expected both to be 3"
    fi

    # The tool path must be a literal, never `${BUILD_PROFILE}`.
    # `publish-river-debug` sets BUILD_PROFILE=dev while cargo's dev profile
    # emits into `debug/`, so an interpolated path there names a directory
    # that cannot exist and the read-back silently short-circuits -- on the
    # task that publishes to the PRODUCTION contract and counter. The other
    # two sites are only accidentally safe (they run under the default
    # `release`), so the rule is uniform rather than per-site.
    # Needle is the tool-path ARGUMENT line -- indented, quoted -- not
    # `web-container-tool" || true`. The sibling scrape above deliberately
    # tolerates `|| true` on its own continuation line, so requiring it here
    # too would let a reformat plus a profile revert pass both checks
    # together. The count is asserted as well: a needle that stops matching
    # anything passes vacuously, which is the failure this whole check exists
    # to avoid.
    tool_needle='^[[:space:]]+"target/native/[^"]*web-container-tool"'
    tool_lines="$(grep -nE "$tool_needle" "$makefile" || true)"
    tool_count="$(grep -cE "$tool_needle" "$makefile" || true)"
    interpolated="$(grep 'BUILD_PROFILE' <<<"$tool_lines" || true)"

    if [[ "$tool_count" -ne 3 ]]; then
        fail "the read-back's tool path is a literal profile" \
            "expected 3 tool-path argument lines, found $tool_count" \
            "(the needle no longer matches, so this check proves nothing)"
    elif [[ -n "$interpolated" ]]; then
        mapfile -t interpolated_lines <<<"$interpolated"
        fail "the read-back's tool path is a literal profile" "${interpolated_lines[@]}"
    else
        ok "the read-back's tool path is a literal profile"
    fi
fi

# ---------------------------------------------------------------------------
# 2. Read-back classification.
#
# The network and the tool are stubbed. What is under test is the decision --
# which of landed / forked / superseded / not-seen / unknown the operator is
# told -- plus the invariant that every one of them exits 0. The parsing the
# stub stands in for is pinned by the web-container-tool unit tests
# (`sign_output_parses_back_and_verifies`), which is the right level for it.
# ---------------------------------------------------------------------------

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

bin="$sandbox/bin"
mkdir -p "$bin"

# Stub fdev: writes whatever $STUB_STATE says into the -o path, or fails when
# STUB_GET_FAILS is set.
cat >"$bin/fdev" <<'STUB'
#!/usr/bin/env bash
out=""
prev=""
for arg in "$@"; do
    if [[ "$prev" == "-o" ]]; then out="$arg"; fi
    prev="$arg"
done
if [[ -n "${STUB_GET_FAILS:-}" ]]; then
    echo "put timed out after 1 peer attempt(s)" >&2
    exit 1
fi
# A node that accepts the connection and then never answers. `fdev --timeout`
# does not cover the WebSocket connect/send/close, so this is what a wedged
# node looks like from the script's side: nothing, indefinitely.
if [[ -n "${STUB_GET_HANGS:-}" ]]; then
    sleep 300
fi
printf '%s' "${STUB_STATE:-some-state-bytes}" >"$out"
exit 0
STUB
chmod +x "$bin/fdev"

# Stub web-container-tool: reports $STUB_VERSION and writes $STUB_ARCHIVE.
tool="$sandbox/web-container-tool"
cat >"$tool" <<'STUB'
#!/usr/bin/env bash
archive_out=""
prev=""
for arg in "$@"; do
    if [[ "$prev" == "--archive-out" ]]; then archive_out="$arg"; fi
    prev="$arg"
done
if [[ -n "${STUB_INSPECT_FAILS:-}" ]]; then
    echo "signature does not verify under the contract parameters" >&2
    exit 1
fi
[[ -n "$archive_out" ]] && printf '%s' "${STUB_ARCHIVE:-}" >"$archive_out"
echo "version=${STUB_VERSION:-1}"
exit 0
STUB
chmod +x "$tool"

params="$sandbox/webapp.parameters"
printf 'x%.0s' {1..32} >"$params"
published="$sandbox/webapp.tar.xz"
printf 'the archive we published' >"$published"

# run_readback <our_version> <publish_rc> [expected_archive]
#   -> stdout in $out, status in $status, wall-clock seconds in $elapsed
#
# The outer `timeout 40` is a harness backstop, not part of what is under
# test: without it, a regression that drops the script's own timeout would
# hang this suite rather than failing it.
run_readback() {
    local archive="${3:-$published}"
    local started=$SECONDS
    out="$(PATH="$bin:$PATH" timeout 40 "$readback" \
        "someContractId" "$params" "$archive" "$1" "$2" "$tool" 2>&1)"
    status=$?
    elapsed=$((SECONDS - started))
}

expect() {
    local name="$1" want="$2"
    if [[ "$status" -ne 0 ]]; then
        fail "$name" "the read-back exited $status; it must always exit 0" "$out"
        return
    fi
    if [[ "$out" != *"$want"* ]]; then
        fail "$name" "expected output to contain: $want" "got:" "$out"
        return
    fi
    ok "$name"
}

# landed: our version, our bytes, clean publish
STUB_VERSION=100 STUB_ARCHIVE="the archive we published" run_readback 100 0
expect "landed is reported when our version and our bytes are live" "LANDED."

# landed after a reported failure: the sentence that breaks the 2026-08-04 chain
STUB_VERSION=100 STUB_ARCHIVE="the archive we published" run_readback 100 1
expect "a publish that timed out but landed says do NOT republish" "Do NOT republish"

# forked: our version, somebody else's bytes
STUB_VERSION=100 STUB_ARCHIVE="a different archive entirely" run_readback 100 0
expect "a version carrying different bytes is reported as a fork" "FORKED."

# superseded -- and it must name the cause where the operator's own publish
# was REJECTED (their counter was behind the network), because in that case
# their changes are not live and "nothing to do" is the wrong instruction.
STUB_VERSION=101 STUB_ARCHIVE="whatever" run_readback 100 0
expect "a higher on-network version is reported as superseded" "SUPERSEDED."
expect "superseded names the case where the publish was rejected" \
    "YOUR CHANGES ARE NOT LIVE"

# not seen yet -- must NOT advise republishing
STUB_VERSION=99 STUB_ARCHIVE="whatever" run_readback 100 0
expect "a lower on-network version is reported as not-yet-seen" "NOT SEEN YET."
if [[ "$out" == *"Republishing on the strength of a single early read"* ]]; then
    ok "not-yet-seen warns against republishing on one early read"
else
    fail "not-yet-seen warns against republishing on one early read" "$out"
fi

# unreadable network: advisory, still exit 0, no verdict
STUB_GET_FAILS=1 run_readback 100 0
expect "an unreadable network is reported without a verdict" "Could not read the network back"

# unverifiable state: an unverified version is not evidence
STUB_INSPECT_FAILS=1 run_readback 100 0
expect "a state that will not verify is reported as unknown" "could not make sense of it"

# a publish failure must not turn into a read-back failure
STUB_GET_FAILS=1 run_readback 100 7
if [[ "$status" -eq 0 ]]; then
    ok "the read-back never propagates the publish's exit code"
else
    fail "the read-back never propagates the publish's exit code" "exited $status"
fi

# A wedged node must not hang the publish. `|| true` at the call site cannot
# rescue a hang -- only a bounded wall clock can -- so this is the check that
# makes "advisory" true in the second sense.
STUB_GET_HANGS=1 RIVER_READBACK_TIMEOUT=1 run_readback 100 0
if [[ "$status" -eq 0 && "$elapsed" -lt 25 ]]; then
    ok "a node that never answers is abandoned, not waited on (${elapsed}s)"
else
    fail "a node that never answers is abandoned, not waited on" \
        "exited $status after ${elapsed}s (expected 0, under 25s)"
fi
# ...and it must SAY it timed out. Asserting only status and elapsed time
# leaves the whole rc-124 branch deletable with the suite still green: the
# script would fall into the generic "fdev execute get failed" arm, which is
# the wrong report and hides that the node never answered at all.
expect "a timed-out read says so, rather than reporting a generic failure" \
    "Gave up reading the network back"

# A junk RIVER_READBACK_TIMEOUT must not abort the script. Its first use is
# arithmetic, so without validation `set -u` kills the run with a raw bash
# error and no footer -- contradicting the always-exits-0-with-a-report
# promise in the script's own header.
STUB_VERSION=100 STUB_ARCHIVE="the archive we published" \
    RIVER_READBACK_TIMEOUT=abc run_readback 100 0
if [[ "$status" -eq 0 && "$out" == *"Ignoring RIVER_READBACK_TIMEOUT"* ]]; then
    ok "a non-numeric read-back timeout falls back with a warning"
else
    fail "a non-numeric read-back timeout falls back with a warning" \
        "exited $status" "$out"
fi

# A missing local archive is a LOCAL problem. Rendering it as FORKED raises
# the scariest verdict the script has over a tooling slip.
STUB_VERSION=100 STUB_ARCHIVE="whatever" run_readback 100 0 "$sandbox/not-here.tar.xz"
if [[ "$out" != *"FORKED."* && "$out" == *"missing or empty locally"* ]]; then
    ok "a missing local archive is reported as unknown, not as a fork"
else
    fail "a missing local archive is reported as unknown, not as a fork" "$out"
fi

# A version comparison whose inputs are broken must not fall through to
# LANDED. The archive here MATCHES on purpose: with a mismatching one the
# fall-through lands on FORKED and the test would pass for the wrong reason,
# hiding exactly the false-reassurance direction this guards.
STUB_VERSION=100 STUB_ARCHIVE="the archive we published" run_readback "" 0
if [[ "$status" -eq 0 && "$out" != *"LANDED."* && "$out" == *"Skipped"* ]]; then
    ok "a non-integer version is reported as unknown, never as LANDED"
else
    fail "a non-integer version is reported as unknown, never as LANDED" \
        "exited $status" "$out"
fi

printf '\n%d checks, %d failures\n' "$checks" "$failures"
[[ "$failures" -eq 0 ]]
