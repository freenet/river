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

# run_readback <our_version> <publish_rc> -> stdout in $out, status in $status
run_readback() {
    out="$(PATH="$bin:$PATH" "$readback" \
        "someContractId" "$params" "$published" "$1" "$2" "$tool" 2>&1)"
    status=$?
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

# superseded
STUB_VERSION=101 STUB_ARCHIVE="whatever" run_readback 100 0
expect "a higher on-network version is reported as superseded" "SUPERSEDED."

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

printf '\n%d checks, %d failures\n' "$checks" "$failures"
[[ "$failures" -eq 0 ]]
