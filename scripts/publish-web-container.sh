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
# 3. RE-READ AFTER PUBLISHING. fdev's exit code is not evidence. The direction
#    we have OBSERVED is a publish that reported `put timed out after 1 peer
#    attempt(s)` and had landed anyway — 2026-08-04. The other direction is not
#    a claim about any mechanism: a zero exit says the node we published THROUGH
#    accepted the PUT, which is not the same as "the bytes users fetch are
#    ours". So the only thing that establishes "our state is live" is fetching
#    it back and comparing the archive byte for byte. Signature validity cannot
#    do it: both forked archives were validly signed.
#
#    An earlier version of this file justified that with "a publish at an
#    already-used version is a no-op SUCCESS". That is WRONG for this contract
#    and should not come back: web-container-contract's `update_state` returns
#    `InvalidUpdateWithInfo` for `version <= current`
#    (contracts/web-container-contract/src/lib.rs), and freenet-core maps a
#    failed PutResponse to an error on the originating node
#    (crates/core/src/operations/put.rs) — which is how the 2026-08-04 operator
#    saw `New state version 30000377 must be higher than current version
#    30000377` at all. The sibling POINTER contract genuinely does accept a
#    stale update silently, by design; that property is its, not this one's.
#
# Every network read here is RETRIED, in both directions. Freenet is eventually
# consistent, so one GET that does not show our state is a single sample, not
# evidence that our state is not there. A stale sample reported as NOT PUBLISHED
# invites precisely the retry that forked the site.
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
#   scripts/publish-web-container.sh --build      # build the inputs first, under
#                                                 # the same lock (what the
#                                                 # cargo-make tasks pass)
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
#   RIVER_WC_ALLOW_UNPROVEN=1       skip the provenance gate in [0] (clean tree,
#                                   on main, at origin/main, CI green on that
#                                   SHA). For one run; never export it.
#   RIVER_WC_GET_TIMEOUT            seconds, default 180
#   RIVER_WC_PUBLISH_TIMEOUT        seconds, default 300
#   RIVER_WC_PREFLIGHT_ATTEMPTS     pre-flight read attempts, default 3
#   RIVER_WC_PREFLIGHT_DELAY        seconds between them, default 15
#   RIVER_WC_READBACK_ATTEMPTS      default 3
#   RIVER_WC_READBACK_DELAY         seconds between read-back attempts, default 15
#
#   The four counts above are validated, not clamped. Attempts below 1 refuse:
#   zero read-back attempts skips the verification this script exists for and
#   reports UNKNOWN about a publish that landed, which is the stale-read-back
#   failure reached from the configuration side.
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

# shellcheck source=scripts/web-container-publish-lib.sh
. "$SCRIPT_DIR/web-container-publish-lib.sh"

die() { echo "" >&2; echo "ERROR: $*" >&2; exit 1; }
say() { echo "  $*"; }

SIGN_ONLY=0
DRY_RUN=0
DO_BUILD=0
while [ $# -gt 0 ]; do
    case "$1" in
        --sign-only) SIGN_ONLY=1; shift ;;
        --dry-run)   DRY_RUN=1; shift ;;
        --build)     DO_BUILD=1; shift ;;
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

GET_TIMEOUT="${RIVER_WC_GET_TIMEOUT:-180}"
PUBLISH_TIMEOUT="${RIVER_WC_PUBLISH_TIMEOUT:-300}"
PREFLIGHT_ATTEMPTS="${RIVER_WC_PREFLIGHT_ATTEMPTS:-3}"
PREFLIGHT_DELAY="${RIVER_WC_PREFLIGHT_DELAY:-15}"
READBACK_ATTEMPTS="${RIVER_WC_READBACK_ATTEMPTS:-3}"
READBACK_DELAY="${RIVER_WC_READBACK_DELAY:-15}"
ALLOW_UNVERIFIED="${RIVER_WC_ALLOW_UNVERIFIED:-0}"
ALLOW_FIRST_PUBLISH="${RIVER_WC_ALLOW_FIRST_PUBLISH:-0}"
ALLOW_UNPROVEN="${RIVER_WC_ALLOW_UNPROVEN:-0}"

# ------------------------------------------------------------- PRECONDITIONS
echo "=============================================================="
echo " River web-container publish — pre-flight"
echo "=============================================================="
echo ""

for tool in fdev cmp git; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found (needed by this script)"
done
for f in "$VERSION_FILE" "$PARAMS_FILE" "$WASM_FILE" "$CONTRACT_ID_FILE"; do
    [ -f "$f" ] || die "$f missing."
done

# A knob that switches a check off is a hole in the check. Validated, never
# clamped: RIVER_WC_READBACK_ATTEMPTS=0 makes the read-back loop run zero times,
# so the script reports UNKNOWN about a publish that plainly landed — the same
# failure as believing a stale read-back, reached from the configuration side.
# A wedged config has to be loud, so it refuses rather than quietly using a
# default the operator did not ask for.
require_count() { # require_count <name> <value> <min>
    case "$2" in
        ''|*[!0-9]*) die "$1 must be a whole number, got '$2'." ;;
    esac
    [ "${#2}" -le 10 ] || die "$1 is implausibly large: '$2'."
    [ "$2" -ge "$3" ] || die "$1 must be at least $3, got '$2'.
Below that it switches off a check this script exists to perform."
}
require_count RIVER_WC_GET_TIMEOUT        "$GET_TIMEOUT"        1
require_count RIVER_WC_PUBLISH_TIMEOUT    "$PUBLISH_TIMEOUT"    1
require_count RIVER_WC_PREFLIGHT_ATTEMPTS "$PREFLIGHT_ATTEMPTS" 1
require_count RIVER_WC_PREFLIGHT_DELAY    "$PREFLIGHT_DELAY"    0
require_count RIVER_WC_READBACK_ATTEMPTS  "$READBACK_ATTEMPTS"  1
require_count RIVER_WC_READBACK_DELAY     "$READBACK_DELAY"     0

CONTRACT_ID="$(tr -d '[:space:]' < "$CONTRACT_ID_FILE")"
[ -n "$CONTRACT_ID" ] || die "$CONTRACT_ID_FILE is empty."
COUNTER="$(tr -d '[:space:]' < "$VERSION_FILE")"
case "$COUNTER" in
    ''|*[!0-9]*) die "$VERSION_FILE does not hold a number: '$COUNTER'" ;;
esac
# The container's version field is a u32 (WebContainerMetadata.version), so a
# counter at u32::MAX has no successor the contract can accept. Checked HERE,
# before anything is written: left unchecked, the run wrote counter+1 into the
# version file and only then failed, leaving a value the contract can never
# take — every later publish wedged until someone hand-edited the file. The
# length test comes first because a long enough string overflows the arithmetic
# that would otherwise judge it.
U32_MAX=4294967295
[ "${#COUNTER}" -le 10 ] || die "$VERSION_FILE holds a number too large to be a version: '$COUNTER'.
Nothing has been written."
if [ "$COUNTER" -ge "$U32_MAX" ]; then
    die "$VERSION_FILE holds $COUNTER, which leaves no version below u32::MAX
($U32_MAX) to sign at — the web container's version field is a u32, so this
contract cannot accept a higher state and no publish can ever succeed.
Nothing has been written. This needs a decision about the contract, not an
edit to the counter file."
fi

# published-contract/ holds the wasm, the parameters AND the id, and nothing
# used to check that the three agree. The id is DERIVED from (wasm,
# parameters), so a stale wasm, a stale parameters file or a stale id file
# means the publish writes to one contract while every check in this script
# measures another: the pre-flight GET, the version floor and the post-publish
# byte comparison would all read an address nobody is writing to, and the run
# would end with a green verdict having published where nobody will look. That
# is worse than the fork this script exists to prevent, because it is silent in
# both directions.
#
# This is NOT the signing-key check in [3]. That one catches a key that is not
# this contract's; this one catches a wasm or an id file that is not. Neither
# stands in for the other.
say "checking published-contract/ agrees with itself"
set +e
DERIVE_OUT="$(fdev get-contract-id --code "$WASM_FILE" --parameters "$PARAMS_FILE" 2>&1)"
DERIVE_RC=$?
set -e
DERIVED_ID="$(printf '%s' "$DERIVE_OUT" | tr -d '[:space:]')"
if [ "$DERIVE_RC" -ne 0 ] || [ -z "$DERIVED_ID" ]; then
    printf '%s\n' "$DERIVE_OUT" | sed 's/^/      /'
    die "could not derive the contract id from $WASM_FILE + $PARAMS_FILE.
Refusing to publish without knowing which contract these bytes address."
fi
if [ "$DERIVED_ID" != "$CONTRACT_ID" ]; then
    die "published-contract/ disagrees with itself.

  $WASM_FILE
+ $PARAMS_FILE
  derive  $DERIVED_ID
  but $CONTRACT_ID_FILE says
          $CONTRACT_ID

Publishing would write to $DERIVED_ID while every check here measured
$CONTRACT_ID. One of the three is stale — re-run
'cargo make update-published-contract' and commit the result."
fi
say "wasm + parameters derive $CONTRACT_ID"

# --------------------------------------------------------------- SINGLE WRITER
#
# Two concurrent runs would race the read-modify-write on the counter and could
# sign two archives at the same version — the fork, arrived at from a different
# direction. [tasks.sign-webapp] has carried a comment saying "wrap in flock if
# you need that" since the counter was introduced; nothing ever did.
#
# The lock lives OUTSIDE the checkout, keyed by contract id. River development
# is worktree-based and this machine routinely carries several River worktrees
# at once, so a lock file inside the repo hands two concurrent publishes two
# DIFFERENT lock files and serialises nothing — a lock that reads as protection
# and is not one. What has to be serialised is "publishes to this contract on
# this machine", and that is exactly what the path below names. (Two different
# UNIX users would still miss each other where $XDG_RUNTIME_DIR is per-user;
# one operator per machine is the case this is built for, and the alternative,
# a fixed name in a world-writable /tmp, trades that for a path another user
# can squat.)
#
# The lock is a file descriptor held open for the life of the script, not a
# re-exec under `flock <file> <command>`. Nothing has to be handed the script's
# own path or arguments — a re-exec through "$0" dies under
# `cd scripts && ./publish-web-container.sh` — and there is no "already locked"
# environment sentinel that an inherited environment could set to switch the
# lock off.
#
# A missing flock WARNS rather than refusing. flock is Linux-only (these tasks
# already build x86_64-unknown-linux-gnu, so that is the expected platform), and
# blocking a release because a lock utility is absent is a worse failure than
# the race it prevents — the race needs two simultaneous publishes by one
# operator on one machine.
LOCK_ID="$(printf '%s' "$CONTRACT_ID" | tr -c 'A-Za-z0-9._-' '_')"
LOCK_FILE="${XDG_RUNTIME_DIR:-/tmp}/river-web-container-$LOCK_ID.lock"
if command -v flock >/dev/null 2>&1; then
    if : >> "$LOCK_FILE" 2>/dev/null; then
        exec 9>>"$LOCK_FILE"
        if flock --nonblock 9; then
            say "single-writer lock held: $LOCK_FILE"
        else
            die "another web-container publish is already running (lock: $LOCK_FILE).
Two concurrent runs race the version counter and can sign two different
archives at the same version. Wait for the other one to finish."
        fi
    else
        echo "WARNING: could not open the lock file $LOCK_FILE —" >&2
        echo "         running WITHOUT the single-writer lock." >&2
    fi
else
    echo "WARNING: flock not found — running WITHOUT the single-writer lock." >&2
    echo "         Do not run a second publish concurrently." >&2
fi

# ------------------------------------------------------------- [0] PROVENANCE
#
# The web container is ONE fixed contract key serving every River user: whatever
# is in this checkout is what every user gets, and there is no per-branch
# staging address to publish to instead. ~/.claude/rules/publish-from-main.md
# names this key explicitly, and river's `main` carries no branch protection, so
# nothing upstream of this script enforces any of it.
#
# These are the four checks scripts/publish-pointer-records.sh makes before its
# own network write. They run for a --sign-only run too: signing burns a version
# and hands back an artifact that `fdev network publish` will take by hand, so
# it is a release step, not a build step. --dry-run runs them as well, because a
# pre-flight that skipped the gate would report a publish as fine that the real
# run then refuses.
echo ""
echo "[0] provenance: clean tree, on main, at origin/main, CI green"
if [ "$ALLOW_UNPROVEN" = "1" ]; then
    echo "  !! provenance gate SKIPPED by RIVER_WC_ALLOW_UNPROVEN=1." >&2
    echo "  !! whatever is in this checkout is what every River user will get." >&2
else
    git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
        || die "$REPO_ROOT is not a git checkout, so nothing here can say which commit
is being published to a key every River user resolves.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."

    # TRACKED modifications, with ONE exemption in ONE direction.
    #
    # A modified tracked file means this checkout differs from the commit whose
    # CI is verified below, which is the whole provenance claim. The counter is
    # the exception, because it is the one tracked file THIS SCRIPT writes by
    # design: after any run that reached [3] it is legitimately ahead of HEAD.
    # Refusing that would refuse the re-run that every non-landed verdict tells
    # the operator to make, and the reflex when a script says "clean checkout"
    # and names one file is `git checkout -- <file>` — which IS the 2026-08-04
    # rollback. A gate must not manufacture pressure toward the incident it
    # exists to prevent.
    #
    # It is safe to exempt because the counter is NOT an input to the archive.
    # It only seeds wc_next_version, whose result is re-floored against the
    # version read off the network, so a counter ahead of HEAD cannot change
    # what users get.
    #
    # STRICTLY GREATER only. A working-tree counter at or below HEAD's is the
    # dangerous direction — something rolled it back — and stays a refusal.
    DIRTY="$(git status --porcelain --untracked-files=no)"
    if [ -n "$DIRTY" ]; then
        DIRTY_OTHER="$(printf '%s\n' "$DIRTY" | grep -v "[[:space:]]${VERSION_FILE}\$" || true)"
        if [ -n "$DIRTY_OTHER" ]; then
            printf '%s\n' "$DIRTY_OTHER" | sed 's/^/    /'
            die "tracked files are modified. Publish only from a clean checkout of main.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."
        fi
        HEAD_COUNTER="$(git show "HEAD:$VERSION_FILE" 2>/dev/null | tr -d '[:space:]' || true)"
        case "$HEAD_COUNTER" in
            ''|*[!0-9]*) die "$VERSION_FILE is modified and HEAD does not hold a number for it.
Refusing to guess whether that is a burned version or a rollback." ;;
        esac
        if [ "${#HEAD_COUNTER}" -le 10 ] && [ "$COUNTER" -gt "$HEAD_COUNTER" ]; then
            say "note: $VERSION_FILE is ahead of HEAD ($HEAD_COUNTER -> $COUNTER)."
            say "      That is a version an earlier run burned. COMMIT it. Never"
            say "      'git checkout' it — that reissues a version that may be live."
        else
            die "$VERSION_FILE is modified and holds $COUNTER, which is not above HEAD's
$HEAD_COUNTER. The counter is forward-only: a value that is not ahead means
something rolled it back, and reissuing a version that has already been
published is the 2026-08-04 fork. Restore it to at least $HEAD_COUNTER."
        fi
    fi

    # UNTRACKED files. The old note here asserted that they "cannot change what
    # is published". That is false: the build reaches files by GLOB, not by git.
    # `--build` runs compress-webapp -> build-ui -> build-tailwind, and
    # ui/assets/tailwind.css carries `@source "../src/**/*.rs"`, so an untracked
    # .rs under ui/src contributes class names to the generated stylesheet that
    # dx bundles and compress-webapp tars into the archive. `dx build` walks
    # ui/assets the same way. Both were verified against this tree.
    #
    # So the demonstrated vectors are refused rather than noted. This does NOT
    # claim to enumerate every glob the build reaches — which is why the note
    # below now says what was checked instead of asserting a universal.
    UNTRACKED="$(git status --porcelain --untracked-files=all | grep '^??' | sed 's/^?? //' || true)"
    UNTRACKED_BUILD_INPUT="$(printf '%s\n' "$UNTRACKED" | grep '^ui/' || true)"
    if [ -n "$UNTRACKED_BUILD_INPUT" ]; then
        printf '%s\n' "$UNTRACKED_BUILD_INPUT" | sed 's/^/    /'
        die "untracked files under ui/ would be built into the published archive.
Tailwind globs ui/src/**/*.rs for class names and dx walks ui/assets, so these
reach the archive without reaching git — the published site would differ from
both HEAD and the artifact CI tested. Commit them or remove them.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."
    fi
    if [ -n "$UNTRACKED" ]; then
        say "note: untracked files present, none under ui/ (the build inputs checked here):"
        printf '%s\n' "$UNTRACKED" | sed 's/^/      /'
    fi

    BRANCH="$(git rev-parse --abbrev-ref HEAD)"
    [ "$BRANCH" = "main" ] || die "on branch '$BRANCH'. Publish only from main.
This contract key is shared, so a feature-branch publish ships the site without
whatever merged to main since the branch was cut — see
~/.claude/rules/publish-from-main.md.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."

    git fetch origin main --quiet \
        || die "could not fetch origin/main, so 'HEAD == origin/main' cannot be checked.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."
    HEAD_SHA="$(git rev-parse HEAD)"
    ORIGIN_SHA="$(git rev-parse origin/main)"
    [ "$HEAD_SHA" = "$ORIGIN_SHA" ] || die "HEAD ($HEAD_SHA) != origin/main ($ORIGIN_SHA). Pull first.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."
    say "HEAD == origin/main == $HEAD_SHA"

    command -v gh >/dev/null 2>&1 || die "gh not found — cannot verify CI on $HEAD_SHA"
    command -v jq >/dev/null 2>&1 || die "jq not found — cannot read the CI status for $HEAD_SHA"
    RUNS="$(gh run list --repo freenet/river --commit "$HEAD_SHA" \
        --json conclusion,status,name 2>/dev/null || echo "")"
    [ -n "$RUNS" ] || die "could not read CI status for $HEAD_SHA.
Refusing to publish on an unknown signal.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."

    # COUNT THE RUNS, not only the bad ones. An empty list yields zero failures,
    # so a `length == 0` test on its own reports "green" for a commit CI never
    # ran on — a guard that cannot fail, which is worse than no guard because it
    # is reassuring. Ask for evidence of success, then for absence of failure.
    TOTAL="$(printf '%s' "$RUNS" | jq 'length')"
    [ "$TOTAL" -gt 0 ] || die "NO CI runs exist for $HEAD_SHA.
That is not the same as green. Either CI has not started, or this commit is not
what you think it is. Refusing to publish.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."

    # Non-SUCCESS conclusions include failure, cancelled and timed_out; a
    # still-running check is also not a green signal.
    BAD="$(printf '%s' "$RUNS" | jq '[.[] | select(.status != "completed" or (.conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral"))] | length')"
    if [ "$BAD" != "0" ]; then
        gh run list --repo freenet/river --commit "$HEAD_SHA" --limit 20 | sed 's/^/    /'
        die "$BAD of $TOTAL CI check(s) on $HEAD_SHA are not green.
Never publish on red or pending CI.
Set RIVER_WC_ALLOW_UNPROVEN=1 for this run if you accept that."
    fi
    say "CI green on $HEAD_SHA ($TOTAL run(s), all successful)"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ------------------------------------------------------------ [0b] BUILD INPUTS
#
# The artifacts are built HERE, inside the lock, instead of being declared as
# cargo-make dependencies of [tasks.publish-river]. cargo-make runs a task's
# dependencies to completion BEFORE its script — so a dependency-built archive
# is produced before this script exists to lock anything. Two overlapping
# `cargo make publish-river` runs therefore both wrote the shared
# target/webapp/webapp.tar.xz outside any lock, and the second could replace the
# archive while the first was signing it, publishing it, or comparing it byte
# for byte against the read-back. The lock covered sign-to-verify but not the
# artifact it was verifying, which is not what "one lock across the whole
# sequence" means. (It degrades to a confusing failure rather than a fork: the
# first run publishes B's archive under metadata signed for A's, validate_state
# rejects it, and the read-back says not-landed.)
if [ "$DO_BUILD" -eq 1 ]; then
    echo ""
    echo "[0b] building the inputs, under the same lock"
    command -v cargo >/dev/null 2>&1 || die "--build was requested but cargo is not on PATH."
    # --env, not an inherited variable. cargo-make's [env] block OVERRIDES an
    # environment variable of the same name (verified against cargo-make
    # 0.37.24), so a nested `cargo make` would rebuild publish-river-debug at
    # the global default profile and then look for a tool built somewhere else.
    cargo make --env BUILD_PROFILE="${BUILD_PROFILE:-release}" web-container-build-inputs \
        || die "the build failed. Nothing has been signed and the counter is untouched."
    # [0] vouched for a clean tracked tree and the build ran after it. Nothing
    # in the build is supposed to write a tracked path; if that stops being
    # true, the commit whose CI we verified is no longer the thing we publish.
    if [ "$ALLOW_UNPROVEN" != "1" ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        git status --short --untracked-files=no | sed 's/^/    /'
        die "the build modified tracked files, so what is about to be published is no
longer the commit whose CI was verified in [0]. Nothing has been signed."
    fi
fi

[ -x "$TOOL" ] || die "$TOOL not found. Run via cargo make so it gets built."
[ -s "$ARCHIVE" ] || die "$ARCHIVE missing or empty. Run 'cargo make compress-webapp' first."

# --------------------------------------------- [1] READ THE NETWORK, BEFORE SIGNING
echo ""
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

# One failed GET is a missing sample, not a network answer, and refusing a
# legitimate publish because a single read timed out is a self-inflicted
# outage — so ask again before concluding `unknown`. `absent` is deliberately
# NOT retried here: it is a definite answer FROM the node, and whether to
# believe it for this contract is wc_preflight_decision's call rather than a
# question of asking harder.
preflight_attempt=1
while :; do
    read_network_version "$WORK/onnet.bin" "$WORK/onnet.log"
    [ "$NET_STATUS" = "unknown" ] || break
    [ "$preflight_attempt" -lt "$PREFLIGHT_ATTEMPTS" ] || break
    tail -3 "$WORK/onnet.log" | sed 's/^/      /'
    say "attempt $preflight_attempt/$PREFLIGHT_ATTEMPTS did not learn the version; retrying in ${PREFLIGHT_DELAY}s"
    preflight_attempt=$((preflight_attempt + 1))
    if [ "$PREFLIGHT_DELAY" -gt 0 ]; then
        sleep "$PREFLIGHT_DELAY"
    fi
done

case "$NET_STATUS" in
    known)   say "the network holds version $NET_VERSION (signature verified under our parameters)" ;;
    absent)  say "the node answered: Contract not found" ;;
    unknown) tail -3 "$WORK/onnet.log" | sed 's/^/      /'
             say "could NOT determine the on-network version ($preflight_attempt attempt(s))" ;;
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
# The warning matters MOST in --sign-only mode, not least: that is the one path
# with no read-back at all, and it still burns a version and hands back a signed
# artifact `fdev network publish` will take.
if [ "$NET_STATUS" != "known" ]; then
    echo ""
    echo "  !! proceeding WITHOUT a verified on-network version, by explicit override." >&2
    if [ "$SIGN_ONLY" -eq 1 ]; then
        echo "  !! --sign-only does NOT read back, so NOTHING here will check this" >&2
        echo "  !! version against the network. If it is not above the live one," >&2
        echo "  !! publishing the artifact this produces is the 2026-08-04 fork." >&2
    else
        echo "  !! the read-back after the publish is the only remaining check." >&2
    fi
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

KEY_ARGS=()
if [ -n "${RIVER_WC_KEY_FILE:-}" ]; then
    KEY_ARGS=(--key-file "$RIVER_WC_KEY_FILE")
fi

# The parameters ARE the contract's identity: the contract ID is derived from
# (wasm, parameters). If the key is not the key this contract belongs to, the
# publish would go to a different contract entirely and every check after it
# would be measuring the wrong thing.
#
# Checked BEFORE the counter is written. `export-parameters` reads the key and
# writes one file into $WORK: it signs nothing and burns nothing. The wrong key
# has to cost an error message rather than a version — the old order wrote the
# counter first, so a wrong-key abort consumed a version and published nothing.
set +e
"$TOOL" export-parameters --parameters "$WORK/key-check.parameters" \
    ${KEY_ARGS[@]+"${KEY_ARGS[@]}"} >"$WORK/key-check.log" 2>&1
KEY_CHECK_RC=$?
set -e
if [ "$KEY_CHECK_RC" -ne 0 ]; then
    sed 's/^/      /' "$WORK/key-check.log"
    die "could not read the signing key. Check ${RIVER_WC_KEY_FILE:-~/.config/river/web-container-keys.toml}."
fi
cmp -s "$WORK/key-check.parameters" "$PARAMS_FILE" || die "the key we would sign with does not match $PARAMS_FILE.
Signing with the wrong key publishes to a DIFFERENT contract ID. Check
${RIVER_WC_KEY_FILE:-~/.config/river/web-container-keys.toml}.
Nothing has been signed and the counter is untouched."
say "the signing key owns $CONTRACT_ID"

# Forward-only. Written BEFORE signing so a crash between here and the publish
# leaves the counter ahead rather than behind: ahead costs a gap, behind costs
# a reissued version.
echo "$VERSION" > "$VERSION_FILE"
# Remove the rollback snapshot the old flow left behind, if a stale one exists.
rm -f "$VERSION_FILE.prev"

"$TOOL" sign \
    --input "$ARCHIVE" \
    --output "$METADATA" \
    --parameters "$SIGNED_PARAMS" \
    --version "$VERSION" \
    ${KEY_ARGS[@]+"${KEY_ARGS[@]}"}

# The parameters `sign` actually wrote, not the ones `export-parameters` said it
# would: the same claim, re-checked against the artifact about to be published.
cmp -s "$SIGNED_PARAMS" "$PARAMS_FILE" || die "the key that just signed does not match $PARAMS_FILE.
Signing with the wrong key publishes to a DIFFERENT contract ID. Check
~/.config/river/web-container-keys.toml."
say "signed version $VERSION with the key that owns $CONTRACT_ID"

if [ "$SIGN_ONLY" -eq 1 ]; then
    echo ""
    # NOT "commit only after a successful publish" — that was the pre-rollback
    # doctrine, and it implies discarding the counter when the publish fails,
    # which is the 2026-08-04 rollback. An unpublished burn costs a gap; a
    # reissue forks the site. --sign-only is also the one path with no
    # read-back, so nothing here can tell you which of the two you are in.
    echo "--sign-only: not publishing. COMMIT $VERSION_FILE — version $VERSION is"
    echo "burned whether or not it is ever published, and reissuing it is the fork."
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
echo "    'the publish returned OK' is not evidence: it says the node we published"
echo "    THROUGH accepted the PUT, not that the bytes users fetch are ours. And a"
echo "    publish that reports a timeout may have landed anyway (2026-08-04)."
echo "    Only the bytes on the network settle it."

# A non-zero publish is the case where the PUT is most likely still in flight —
# 2026-08-04 exactly. Reading back the instant fdev gives up samples the network
# before it could have converged, and a stale sample there reads as NOT
# PUBLISHED, which is the report that invites the retry that forks the site.
if [ "$PUB_RC" -ne 0 ] && [ "$READBACK_DELAY" -gt 0 ]; then
    say "waiting ${READBACK_DELAY}s before the first read-back (fdev reported a failure;"
    say "the PUT may still be in flight, and an immediate read would be a stale read)"
    sleep "$READBACK_DELAY"
fi

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
    # See wc_readback_is_final. 'not-landed' and 'unknown' are the same claim in
    # two shapes — "we have not seen our state yet" — and on an eventually
    # consistent network that is indistinguishable from a read that was simply
    # too early. Only a settled answer stops the loop.
    if [ "$(wc_readback_is_final "$OUTCOME")" = "yes" ]; then
        break
    fi
    attempt=$((attempt + 1))
    if [ "$attempt" -le "$READBACK_ATTEMPTS" ] && [ "$READBACK_DELAY" -gt 0 ]; then
        sleep "$READBACK_DELAY"
    fi
done
READS_MADE="$attempt"
[ "$READS_MADE" -le "$READBACK_ATTEMPTS" ] || READS_MADE="$READBACK_ATTEMPTS"

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
        # `git add` alone leaves the file staged-but-uncommitted, which still
        # reads as modified to the gate in [0] and to anyone else looking.
        echo "  Commit the counter:  git commit -m 'chore: bump web-container version' $VERSION_FILE"
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
        echo " NOT PUBLISHED — the network is still at $BACK_VERSION after $READS_MADE read(s)"
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
        echo " UNKNOWN — could not read the state back after $READS_MADE read(s)"
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
# Every non-landed verdict above tells the operator to re-run. Say what has to
# happen first, in the one place all four arms pass through — because the
# alternative the operator reaches for is `git checkout` on the counter, and
# that is the 2026-08-04 rollback wearing the clothes of tidying up.
if [ "$OUTCOME" != "landed" ]; then
    echo ""
    echo "  BEFORE re-running: commit $VERSION_FILE (now $VERSION)."
    echo "  Do NOT 'git checkout' it. $VERSION may already have been seen by a"
    echo "  peer, and reissuing a used version is what forked the site."
fi
echo ""
exit "$EXIT_CODE"
