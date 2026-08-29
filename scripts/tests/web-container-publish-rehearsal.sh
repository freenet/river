#!/usr/bin/env bash
# Rehearse scripts/publish-web-container.sh end to end against a STUB node.
#
# The decision logic is unit-tested in web-container-publish-lib-test.sh. This
# covers the wiring around it, which is where a publish script actually breaks:
# the fdev argument order, the `Contract not found` string it keys absence off,
# the `version=` line it parses, whether the counter really is forward-only,
# whether the verdict comes out of the read-back rather than the exit code,
# whether the single-writer lock is a lock and covers the artifact build, and
# whether the provenance gate refuses what it says it refuses. None of that is
# reachable from a pure function, and none of it can be exercised against the
# real network (the web container's key is shared and publishing to it from a
# test is out of bounds).
#
# So: a throwaway repo layout — a real git repo, with a real local bare origin,
# because the provenance gate is part of the wiring — a throwaway signing key,
# and `fdev`, `gh` and `cargo` on PATH that are shell scripts. The real publish
# script and the real web-container-tool run unmodified.
#
# Scenario 2 is the 2026-08-04 incident (River commit 1032d373) reproduced:
# a publish that reports a timeout and lands anyway.
#
# NOTE for anyone adding a scenario: this file runs under `set -uo pipefail`
# with NO `set -e`, so a setup step that fails does not abort the scenario — it
# silently mis-runs it and can still report a pass. Assert that your setup
# happened rather than assuming it did. `seed_network` and `git_sync` check
# their own postconditions for exactly this reason.
#
# Run: ./scripts/tests/web-container-publish-rehearsal.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PASS=0
FAIL=0

# A missing interpreter used to `exit 0`. This is the ONLY end-to-end coverage
# of a release path, so reporting success because the machine could not run it
# is a green signal that no input can turn red — the exact shape this suite
# exists to catch in the script. Absence is now a failure; the skip is opt-in
# and must never be set in CI.
missing_tool() {
    if [ "${RIVER_WC_REHEARSAL_ALLOW_SKIP:-0}" = "1" ]; then
        echo "SKIP: $1 not available (RIVER_WC_REHEARSAL_ALLOW_SKIP=1)"
        exit 0
    fi
    echo "FAIL: $1 not available."
    echo "      This is the only end-to-end coverage of the release path, so a"
    echo "      missing interpreter is a failure, not a pass. Install it, or set"
    echo "      RIVER_WC_REHEARSAL_ALLOW_SKIP=1 to skip on purpose (never in CI)."
    exit 1
}
command -v python3 >/dev/null 2>&1 || missing_tool python3
command -v git     >/dev/null 2>&1 || missing_tool git
# The provenance gate reads CI status with `gh ... --json | jq`. gh is stubbed
# below; jq is not, because the script's own jq expressions are under test.
command -v jq      >/dev/null 2>&1 || missing_tool jq

# ------------------------------------------------------------------- the tool

# CI sets CARGO_TARGET_DIR, so the binary is not necessarily under
# $REPO_ROOT/target. Look where cargo will actually have put it.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
TOOL_SRC="${RIVER_WC_TOOL:-}"
if [ -z "$TOOL_SRC" ]; then
    for candidate in \
        "$TARGET_DIR/native/x86_64-unknown-linux-gnu/release/web-container-tool" \
        "$TARGET_DIR/native/x86_64-unknown-linux-gnu/debug/web-container-tool" \
        "$TARGET_DIR/release/web-container-tool" \
        "$TARGET_DIR/debug/web-container-tool"
    do
        [ -x "$candidate" ] && { TOOL_SRC="$candidate"; break; }
    done
fi
if [ -z "$TOOL_SRC" ]; then
    echo "building web-container-tool..."
    (cd "$REPO_ROOT" && cargo build -p web-container-tool) || {
        echo "FAIL: could not build web-container-tool"; exit 1; }
    TOOL_SRC="$TARGET_DIR/debug/web-container-tool"
fi
[ -x "$TOOL_SRC" ] || { echo "FAIL: no web-container-tool at $TOOL_SRC"; exit 1; }

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

mkdir -p "$SANDBOX/scripts/tests" \
         "$SANDBOX/published-contract" \
         "$SANDBOX/target/webapp" \
         "$SANDBOX/target/native/x86_64-unknown-linux-gnu/release" \
         "$SANDBOX/bin" \
         "$SANDBOX/node"
cp "$REPO_ROOT/scripts/publish-web-container.sh" "$SANDBOX/scripts/"
cp "$REPO_ROOT/scripts/web-container-publish-lib.sh" "$SANDBOX/scripts/"
cp "$TOOL_SRC" "$SANDBOX/target/native/x86_64-unknown-linux-gnu/release/web-container-tool"
TOOL="$SANDBOX/target/native/x86_64-unknown-linux-gnu/release/web-container-tool"

# ----------------------------------------------------------- throwaway keys
# Never the production key at ~/.config/river/web-container-keys.toml. The
# publish script's parameters check would refuse it here anyway, since the
# sandbox's committed parameters are this key's.
KEY_FILE="$SANDBOX/keys.toml"
"$TOOL" generate --output "$KEY_FILE" >/dev/null
"$TOOL" export-parameters --parameters "$SANDBOX/published-contract/webapp.parameters" \
    --key-file "$KEY_FILE" >/dev/null
# A second key that owns a DIFFERENT contract, for the wrong-key scenario.
OTHER_KEY="$SANDBOX/other-keys.toml"
"$TOOL" generate --output "$OTHER_KEY" >/dev/null

printf 'not a real wasm' > "$SANDBOX/published-contract/web_container_contract.wasm"
echo "30000000" > "$SANDBOX/published-contract/contract-version.txt"

# ----------------------------------------------------------------- stub fdev
# Behaviour is driven entirely by files under $SANDBOX/node:
#   state.bin     the packed state the "network" holds (absent = not found)
#   get_modes     whitespace-separated queue of ok | notfound | timeout, one
#                 consumed per GET, the last one repeating — so a pre-flight
#                 read and a post-publish read can behave differently
#   publish_rc    exit code the publish reports
#   publish_lands 1 = the publish updates state.bin, 0 = it is a no-op
#   stale_reads   how many GETs AFTER a landing publish still serve the state
#                 as it was BEFORE it. Freenet is eventually consistent, so a
#                 read taken too early legitimately shows the old state; the
#                 stub has to be able to represent that or the read-back retry
#                 has nothing to be right about.
#
# publish_rc and publish_lands are INDEPENDENT knobs on purpose: the script must
# not trust the exit code in either direction, so the suite has to be able to
# present any combination of the two. Do not read a combination as a claim about
# how the real contract behaves. In particular, a PUT at an already-used version
# is NOT a silent success — `update_state` returns InvalidUpdateWithInfo and
# freenet-core surfaces that as an error to the publishing node, which is how
# the 2026-08-04 operator saw the rejection message.
#
# `get-contract-id` derives an id from the bytes it is given, exactly as the
# real one does, so a stale wasm or a stale parameters file genuinely produces a
# different id.
cat > "$SANDBOX/bin/fdev" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
args=("$@")
# Skip the MODE positional if present.
[ "${args[0]:-}" = "network" ] && args=("${args[@]:1}")

case "${args[0]:-}" in
  get-contract-id)
    code=""; params=""
    i=1
    while [ $i -lt ${#args[@]} ]; do
      case "${args[$i]}" in
        --code)       code="${args[$((i+1))]}"; i=$((i+2)) ;;
        --parameters) params="${args[$((i+1))]}"; i=$((i+2)) ;;
        *) i=$((i+1)) ;;
      esac
    done
    if [ ! -f "$code" ] || [ ! -f "$params" ]; then
      echo "Error: cannot read code/parameters" >&2; exit 1
    fi
    echo "ID$(cat "$code" "$params" | md5sum | cut -c1-24)"
    exit 0
    ;;
esac

NODE="${STUB_NODE_DIR:?}"
case "${args[0]:-}" in
  execute)
    # execute get [--timeout N] [-o FILE] KEY
    out=""
    i=2
    while [ $i -lt ${#args[@]} ]; do
      case "${args[$i]}" in
        -o|--output) out="${args[$((i+1))]}"; i=$((i+2)) ;;
        --timeout)   i=$((i+2)) ;;
        *)           i=$((i+1)) ;;
      esac
    done
    read -r -a modes < "$NODE/get_modes"
    mode="${modes[0]}"
    if [ "${#modes[@]}" -gt 1 ]; then
      printf '%s\n' "${modes[*]:1}" > "$NODE/get_modes"
    fi
    case "$mode" in
      notfound) echo "Error: Contract not found" >&2; exit 1 ;;
      timeout)  echo "Error: operation timed out after 180s" >&2; exit 1 ;;
    esac
    # A read that is too early to have seen the last publish.
    src="$NODE/state.bin"
    left="$(cat "$NODE/stale_left" 2>/dev/null || echo 0)"
    if [ "$left" -gt 0 ]; then
      echo $((left - 1)) > "$NODE/stale_left"
      src="$NODE/stale.bin"
    fi
    if [ ! -s "$src" ]; then
      echo "Error: Contract not found" >&2; exit 1
    fi
    cp "$src" "$out"
    exit 0
    ;;
  publish)
    archive=""; metadata=""
    i=1
    while [ $i -lt ${#args[@]} ]; do
      case "${args[$i]}" in
        --webapp-archive)  archive="${args[$((i+1))]}"; i=$((i+2)) ;;
        --webapp-metadata) metadata="${args[$((i+1))]}"; i=$((i+2)) ;;
        *) i=$((i+1)) ;;
      esac
    done
    # Whatever the network held a moment ago is what an early read still sees.
    if [ -s "$NODE/state.bin" ]; then cp "$NODE/state.bin" "$NODE/stale.bin"; fi
    cp "$NODE/stale_reads" "$NODE/stale_left" 2>/dev/null || echo 0 > "$NODE/stale_left"
    if [ -s "$NODE/collision.bin" ]; then
      # A state that is not ours, planted at (or above) the version we are about
      # to publish. Only our own key can sign one the contract accepts, so at
      # our own version this is an earlier run of this pipeline — the 2026-08-04
      # fork; above it, it is something else publishing to this contract.
      cp "$NODE/collision.bin" "$NODE/state.bin"
      exit "$(cat "$NODE/publish_rc")"
    fi
    if [ "$(cat "$NODE/publish_lands")" = "1" ]; then
      python3 - "$metadata" "$archive" "$NODE/incoming.bin" <<'PY'
import sys, struct
meta = open(sys.argv[1],'rb').read()
arch = open(sys.argv[2],'rb').read()
open(sys.argv[3],'wb').write(
    struct.pack('>Q', len(meta)) + meta + struct.pack('>Q', len(arch)) + arch)
PY
      # The container's own gate: `update_state` rejects
      # `version <= current_version`.
      new="$("$STUB_TOOL" inspect --state "$NODE/incoming.bin" | sed -n 's/^version=//p')"
      cur="$(cat "$NODE/version" 2>/dev/null || echo 0)"
      if [ ! -s "$NODE/state.bin" ] || [ "$new" -gt "$cur" ]; then
        cp "$NODE/incoming.bin" "$NODE/state.bin"
        echo "$new" > "$NODE/version"
      fi
    fi
    rc="$(cat "$NODE/publish_rc")"
    [ "$rc" = "0" ] || echo "Error: put timed out after 1 peer attempt(s)" >&2
    exit "$rc"
    ;;
esac
echo "stub fdev: unhandled: $*" >&2
exit 127
STUB
chmod +x "$SANDBOX/bin/fdev"

# ------------------------------------------------------------------- stub gh
# Only `gh run list` is reached, and only from the provenance gate. The JSON it
# prints is whatever $SANDBOX/node/ci_runs holds, so a scenario can present a
# green CI, a red one, or the "no runs at all" case that a `length == 0` test
# would have called green.
cat > "$SANDBOX/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
NODE="${STUB_NODE_DIR:?}"
if [ "${1:-}" = "run" ] && [ "${2:-}" = "list" ]; then
  case " $* " in
    *" --json "*) cat "$NODE/ci_runs" ;;
    *)            echo "(stub gh) run listing" ;;
  esac
  exit 0
fi
echo "stub gh: unhandled: $*" >&2
exit 127
STUB
chmod +x "$SANDBOX/bin/gh"

# ---------------------------------------------------------------- stub cargo
# The publish script runs `cargo make web-container-build-inputs` from inside
# the lock when given --build. Recording the invocation is what lets a scenario
# assert the build did NOT run when the lock was already held.
#
# `cargo_dirties_tree=1` makes it write a TRACKED fixture file, which is the
# only way to reach the post-build clean-tree re-check: the provenance gate runs
# before the build, so that re-check is the guard on the seam between the two.
cat > "$SANDBOX/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
NODE="${STUB_NODE_DIR:?}"
printf '%s\n' "$*" >> "$NODE/cargo.log"
if [ "$(cat "$NODE/cargo_dirties_tree" 2>/dev/null || echo 0)" = "1" ]; then
    printf 'a build product that should not be here\n' >> "${STUB_TRACKED_FILE:?}"
fi
exit 0
STUB
chmod +x "$SANDBOX/bin/cargo"

export STUB_NODE_DIR="$SANDBOX/node"
export STUB_TOOL="$TOOL"
# A tracked, inert fixture file the stub cargo can dirty on demand. Tracked so
# it reaches the post-build clean-tree re-check; inert so nothing else reads it.
printf 'ui source\n' > "$SANDBOX/ui-source.txt"
export STUB_TRACKED_FILE="$SANDBOX/ui-source.txt"
# The harness prepends its own bin/ to PATH, so from here down the SYSTEM cargo
# is shadowed by the stub above — and the stub exits 0 having built nothing.
# That is safe today for one reason only: the sole cargo invocation that reaches
# it is the `--build` call inside the publish script, and the web-container-tool
# build at the top of this file happens BEFORE this line.
#
# The failure mode to recognise, because it does not announce itself: a future
# scenario that needs a REAL cargo gets the stub instead. It will not fail with
# "command not found" — it will succeed instantly, and the scenario will then
# fail somewhere else entirely, complaining about a missing artifact or passing
# for a reason that has nothing to do with what it meant to test. If you add a
# scenario needing a real build, invoke it by absolute path or move it above
# this export.
export PATH="$SANDBOX/bin:$PATH"

# The id the sandbox's wasm + parameters actually derive, written where the
# script expects to read it. Deriving it rather than inventing a string is what
# lets a scenario make published-contract/ inconsistent by touching the wasm.
CONTRACT_ID="$(fdev get-contract-id \
    --code "$SANDBOX/published-contract/web_container_contract.wasm" \
    --parameters "$SANDBOX/published-contract/webapp.parameters")"
case "$CONTRACT_ID" in
    ID*) : ;;
    *) echo "FAIL: setup could not derive a contract id (got '$CONTRACT_ID')"; exit 1 ;;
esac
echo "$CONTRACT_ID" > "$SANDBOX/published-contract/contract-id.txt"

# The lock path is derived here INDEPENDENTLY of the script, from the rule the
# script documents: outside the checkout, keyed by contract id. That is the
# point of deriving it twice — if the lock moves back inside the repo (where
# two worktrees get two different files and it serialises nothing), the
# scenario that holds this path stops conflicting and the test fails.
LOCK_PATH="${XDG_RUNTIME_DIR:-/tmp}/river-web-container-$CONTRACT_ID.lock"

# ---------------------------------------------------- git provenance fixture
# The publish script refuses to sign or publish unless this is a clean checkout
# of main, at origin/main, with green CI on that SHA — because the web
# container is ONE key serving every user and there is no per-branch address to
# publish to instead. That gate is wiring, so it is rehearsed like the rest: a
# real repo with a real (local, bare) origin.
UPSTREAM="$SANDBOX/upstream.git"
git init -q --bare "$UPSTREAM"
git -C "$SANDBOX" init -q
git -C "$SANDBOX" symbolic-ref HEAD refs/heads/main
git -C "$SANDBOX" config user.email rehearsal@example.invalid
git -C "$SANDBOX" config user.name  "publish rehearsal"
cat > "$SANDBOX/.git/info/exclude" <<'EXCLUDE'
/target/
/node/
/bin/
/upstream.git/
/keys.toml
/other-keys.toml
/seed.*
/collide.*
/lock.*
/git-sync.log
EXCLUDE
git -C "$SANDBOX" add scripts published-contract ui-source.txt >/dev/null
git -C "$SANDBOX" commit -qm "sandbox checkout"
git -C "$SANDBOX" remote add origin "$UPSTREAM"
git -C "$SANDBOX" push -q origin main

# ------------------------------------------------------------------- helpers
check() {
    if [ "$2" = "$3" ]; then PASS=$((PASS + 1)); else
        FAIL=$((FAIL + 1))
        echo "FAIL: $1"
        echo "      expected: $2"
        echo "      actual:   $3"
    fi
}
setup_failed() { FAIL=$((FAIL + 1)); echo "SETUP FAIL: $*"; }
contains() { # contains <label> <needle>   (searches $RUN_OUT)
    case "$RUN_OUT" in
        *"$2"*) check "$1" yes yes ;;
        *)      check "$1" yes no  ;;
    esac
}
not_zero() { check "$1" "no" "$([ "$RUN_RC" -eq 0 ] && echo yes || echo no)"; }

# Fold whatever a scenario just wrote into the committed state, and make
# origin/main agree. Checks its own postcondition: nothing in this file has
# `set -e`, so a silent failure here would leave a scenario testing the
# provenance gate instead of whatever it meant to test.
git_sync() {
    git -C "$SANDBOX" add -u >/dev/null
    # --allow-empty: from scenario 27 on, the fixture has a second commit, and a
    # scenario that restores the tree to exactly its parent's state makes the
    # amend "empty" and fails. Without this the sync silently did nothing and
    # left the counter staged — which is how the postcondition check below
    # earned itself twice.
    git -C "$SANDBOX" commit -q --amend --no-edit --allow-empty >"$SANDBOX/git-sync.log" 2>&1 || \
        setup_failed "git_sync amend failed: $(tr '\n' ';' < "$SANDBOX/git-sync.log")"
    git -C "$SANDBOX" push -q --force origin main
    if [ -n "$(git -C "$SANDBOX" status --porcelain --untracked-files=no)" ]; then
        setup_failed "git_sync left tracked files modified: $(git -C "$SANDBOX" status --porcelain --untracked-files=no | tr '\n' ';')"
    elif [ "$(git -C "$SANDBOX" rev-parse HEAD)" != "$(git -C "$SANDBOX" rev-parse origin/main)" ]; then
        setup_failed "git_sync left HEAD != origin/main"
    fi
}

network_version_now() {
    "$TOOL" inspect --state "$SANDBOX/node/state.bin" \
        --parameters "$SANDBOX/published-contract/webapp.parameters" \
        | sed -n 's/^version=//p'
}

# Put a signed state at $1 carrying archive bytes $2 onto the stub network.
seed_network() {
    local version="$1" body="$2" got
    local a="$SANDBOX/seed.tar.xz" m="$SANDBOX/seed.metadata" p="$SANDBOX/seed.parameters"
    printf '%s' "$body" > "$a"
    "$TOOL" sign --input "$a" --output "$m" --parameters "$p" \
        --version "$version" --key-file "$KEY_FILE" >/dev/null
    echo "$version" > "$SANDBOX/node/version"
    python3 - "$m" "$a" "$SANDBOX/node/state.bin" <<'PY'
import sys, struct
meta = open(sys.argv[1],'rb').read()
arch = open(sys.argv[2],'rb').read()
open(sys.argv[3],'wb').write(
    struct.pack('>Q', len(meta)) + meta + struct.pack('>Q', len(arch)) + arch)
PY
    got="$(network_version_now)"
    [ "$got" = "$version" ] || setup_failed "seed_network wanted $version, the stub network holds '$got'"
}

# run <counter> <archive-body> [args...] -> sets RUN_RC / RUN_OUT
#
# Knobs a scenario can set for one call:
#   RUN_SYNC=0   do NOT reconcile the git fixture first (leaves the counter
#                write showing as a dirty tracked file)
#   RB_ATTEMPTS  read-back attempts (default 1)
#   USE_KEY      signing key file (default: the key that owns the contract)
run() {
    local counter="$1" body="$2"; shift 2
    echo "$counter" > "$SANDBOX/published-contract/contract-version.txt"
    printf '%s' "$body" > "$SANDBOX/target/webapp/webapp.tar.xz"
    if [ "${RUN_SYNC:-1}" = "1" ]; then git_sync; fi
    RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="${USE_KEY:-$KEY_FILE}" \
        RIVER_WC_READBACK_ATTEMPTS="${RB_ATTEMPTS:-1}" RIVER_WC_READBACK_DELAY=0 \
        RIVER_WC_PREFLIGHT_DELAY=0 \
        ./scripts/publish-web-container.sh "$@" 2>&1)"
    RUN_RC=$?
}
counter_now() { tr -d '[:space:]' < "$SANDBOX/published-contract/contract-version.txt"; }

reset_node() {
    echo "ok" > "$SANDBOX/node/get_modes"
    echo "0"  > "$SANDBOX/node/publish_rc"
    echo "1"  > "$SANDBOX/node/publish_lands"
    echo "0"  > "$SANDBOX/node/stale_reads"
    echo "0"  > "$SANDBOX/node/cargo_dirties_tree"
    : > "$SANDBOX/node/cargo.log"
    rm -f "$SANDBOX/node/state.bin" "$SANDBOX/node/version" \
          "$SANDBOX/node/collision.bin" "$SANDBOX/node/stale.bin" \
          "$SANDBOX/node/stale_left"
    printf '%s' '[{"name":"build","status":"completed","conclusion":"success"}]' \
        > "$SANDBOX/node/ci_runs"
}

# ------------------------------------------------------------------ scenarios

echo "--- 1: an ordinary publish"
reset_node
seed_network 30000384 "build 384"
run 30000385 "build 386"
check "exits 0"                 "0"        "$RUN_RC"
check "signs counter+1"         "30000386" "$(counter_now)"
check "the network took it"     "30000386" "$(network_version_now)"
contains "reports PUBLISHED" "PUBLISHED"

echo "--- 2: the 2026-08-04 incident — a publish that times out and lands anyway"
reset_node
seed_network 30000376 "build 376"
echo "1" > "$SANDBOX/node/publish_rc"     # fdev reports failure...
echo "1" > "$SANDBOX/node/publish_lands"  # ...but the PUT landed
run 30000376 "build A of the UI"
check "exits 0 despite fdev failing"  "0"        "$RUN_RC"
check "the state IS live"             "30000377" "$(network_version_now)"
check "the counter is NOT rolled back" "30000377" "$(counter_now)"
contains "says so out loud" "despite fdev exiting"

echo "--- 3: the retry that forked the site is now impossible"
# Same counter the old rollback would have restored, and a DIFFERENT archive —
# the non-reproducible rebuild. The floor must lift it off 30000377.
reset_node
seed_network 30000377 "build A of the UI"
run 30000376 "build B of the UI"
check "exits 0"                      "0"        "$RUN_RC"
check "does NOT reissue 30000377"    "30000378" "$(counter_now)"
check "the network is at 30000378"   "30000378" "$(network_version_now)"

echo "--- 4: an unreadable network refuses BEFORE signing"
reset_node
seed_network 30000384 "build 384"
echo "timeout" > "$SANDBOX/node/get_modes"
run 30000385 "build 386"
not_zero "does not exit 0"
check "the counter is untouched"     "30000385" "$(counter_now)"
contains "explains why" "did not learn"

echo "--- 5: 'Contract not found' is not proof of absence for THIS contract"
reset_node
echo "notfound" > "$SANDBOX/node/get_modes"
run 30000385 "build 386"
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
# Assert on the line the script prints ONLY when it classified the answer as
# `absent`. The bare string "Contract not found" is also in the stub's own
# stderr, which lands in $RUN_OUT whatever the script concluded — so matching
# that would pass even if absence detection were broken.
contains "classifies the answer as absence" "the node answered: Contract not found"

echo "--- 6: the override publishes, and the read-back still judges it"
# Every pre-flight read fails, the operator overrides, and the node we publish
# through reports success while our bytes never become what the network serves.
# The read-back (which succeeds) is the only thing that can catch that.
reset_node
seed_network 30000384 "build 384"
echo "timeout timeout timeout ok" > "$SANDBOX/node/get_modes"
echo "0" > "$SANDBOX/node/publish_rc"
echo "0" > "$SANDBOX/node/publish_lands"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
git_sync
RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" RIVER_WC_ALLOW_UNVERIFIED=1 \
    RIVER_WC_READBACK_ATTEMPTS=1 RIVER_WC_READBACK_DELAY=0 RIVER_WC_PREFLIGHT_DELAY=0 \
    ./scripts/publish-web-container.sh 2>&1)"
RUN_RC=$?
not_zero "does not exit 0 when our bytes are not live"
check "the counter stays forward"          "30000386" "$(counter_now)"
check "the network never took it"          "30000384" "$(network_version_now)"
contains "reports NOT PUBLISHED" "NOT PUBLISHED"

echo "--- 7: --sign-only signs and stops"
reset_node
seed_network 30000384 "build 384"
run 30000385 "build 386" --sign-only
check "exits 0"                      "0"        "$RUN_RC"
check "bumps the counter"            "30000386" "$(counter_now)"
check "publishes nothing"            "30000384" "$(network_version_now)"

echo "--- 8: --dry-run writes nothing at all"
reset_node
seed_network 30000384 "build 384"
rm -f "$SANDBOX/target/webapp/webapp.metadata"
run 30000385 "build 386" --dry-run
check "exits 0"                      "0"        "$RUN_RC"
check "the counter is untouched"     "30000385" "$(counter_now)"
check "no metadata written"          "no"       "$([ -f "$SANDBOX/target/webapp/webapp.metadata" ] && echo yes || echo no)"

echo "--- 9: a state we cannot verify is refused outright"
reset_node
# Sign the network's state with a DIFFERENT key.
printf 'foreign build' > "$SANDBOX/seed.tar.xz"
"$TOOL" sign --input "$SANDBOX/seed.tar.xz" --output "$SANDBOX/seed.metadata" \
    --parameters "$SANDBOX/seed.parameters" --version 99 --key-file "$OTHER_KEY" >/dev/null
python3 - "$SANDBOX/seed.metadata" "$SANDBOX/seed.tar.xz" "$SANDBOX/node/state.bin" <<'PY'
import sys, struct
meta = open(sys.argv[1],'rb').read()
arch = open(sys.argv[2],'rb').read()
open(sys.argv[3],'wb').write(
    struct.pack('>Q', len(meta)) + meta + struct.pack('>Q', len(arch)) + arch)
PY
run 30000385 "build 386"
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
contains "says the provenance is unclear" "do NOT verify"

echo "--- 10: a state at OUR version carrying bytes that are not ours"
# Both archives are validly signed, so signatures cannot separate them and the
# state summary (just the u32 version) cannot either. Only comparing the bytes
# we published against the bytes we read back can see this.
reset_node
seed_network 30000384 "build 384"
# The floor will pick 30000386; plant a different archive there.
printf 'a DIFFERENT build at the same version' > "$SANDBOX/collide.tar.xz"
"$TOOL" sign --input "$SANDBOX/collide.tar.xz" --output "$SANDBOX/collide.metadata" \
    --parameters "$SANDBOX/collide.parameters" --version 30000386 \
    --key-file "$KEY_FILE" >/dev/null
python3 - "$SANDBOX/collide.metadata" "$SANDBOX/collide.tar.xz" "$SANDBOX/node/collision.bin" <<'PYX'
import sys, struct
meta = open(sys.argv[1],'rb').read()
arch = open(sys.argv[2],'rb').read()
open(sys.argv[3],'wb').write(
    struct.pack('>Q', len(meta)) + meta + struct.pack('>Q', len(arch)) + arch)
PYX
echo "30000386" > "$SANDBOX/node/version"
run 30000385 "build 386"
not_zero "does not exit 0"
contains "reports FORKED" "FORKED"

echo "--- 11: something else published above us"
# The real shape of a rejected publish: the network moved past our version
# between the read and the write, so the contract refuses our state. Our archive
# is not what users are getting and the run must say so rather than exit 0.
reset_node
seed_network 30000384 "build 384"
printf 'somebody elses newer build' > "$SANDBOX/collide.tar.xz"
"$TOOL" sign --input "$SANDBOX/collide.tar.xz" --output "$SANDBOX/collide.metadata" \
    --parameters "$SANDBOX/collide.parameters" --version 30000390 \
    --key-file "$KEY_FILE" >/dev/null
python3 - "$SANDBOX/collide.metadata" "$SANDBOX/collide.tar.xz" "$SANDBOX/node/collision.bin" <<'PYX'
import sys, struct
meta = open(sys.argv[1],'rb').read()
arch = open(sys.argv[2],'rb').read()
open(sys.argv[3],'wb').write(
    struct.pack('>Q', len(meta)) + meta + struct.pack('>Q', len(arch)) + arch)
PYX
echo "1" > "$SANDBOX/node/publish_rc"   # the contract rejected it: an ERROR, not a silent success
run 30000385 "build 386"
not_zero "does not exit 0"
check "the counter stays forward"    "30000386" "$(counter_now)"
contains "reports NOT LIVE" "NOT LIVE"

echo "--- 12: the first read-back is stale, and the publish DID land"
# Freenet is eventually consistent: the network is not obliged to show our
# state on the first ask. Stopping at that first answer reports a publish that
# LANDED as NOT PUBLISHED, and that report is what invites the retry that
# forked the site. So `not-landed` has to keep asking.
reset_node
seed_network 30000384 "build 384"
echo "1" > "$SANDBOX/node/stale_reads"   # one post-publish read sees the OLD state
RB_ATTEMPTS=3 run 30000385 "build 386"
check "exits 0"                      "0"        "$RUN_RC"
check "the state IS live"            "30000386" "$(network_version_now)"
contains "reports PUBLISHED" "PUBLISHED"

echo "--- 13: a read-back that never answers leaves the counter forward"
# Pre-flight reads fine, the publish lands, and then the node stops answering.
# The verdict is UNKNOWN — and the counter must NOT be rolled back, precisely
# because we cannot rule out that the version landed. Rolling it back is what
# handed the 2026-08-04 retry a version that was already in use.
reset_node
seed_network 30000384 "build 384"
echo "ok timeout" > "$SANDBOX/node/get_modes"
RB_ATTEMPTS=2 run 30000385 "build 386"
not_zero "does not exit 0"
check "the counter stays forward"    "30000386" "$(counter_now)"
contains "reports UNKNOWN" "UNKNOWN"

echo "--- 14: a transient pre-flight failure is retried, not treated as a refusal"
# One failed GET is a missing sample, not a network answer. Refusing the whole
# release because the first read timed out is a self-inflicted outage.
reset_node
seed_network 30000384 "build 384"
echo "timeout ok" > "$SANDBOX/node/get_modes"
run 30000385 "build 386"
check "exits 0"                      "0"        "$RUN_RC"
check "used the network as the floor" "30000386" "$(counter_now)"
check "the network took it"          "30000386" "$(network_version_now)"

echo "--- 15: the wrong signing key costs an error, not a version"
# The contract ID is derived from (wasm, parameters), so signing with a key
# that is not this contract's would publish to a DIFFERENT contract. The check
# has to happen BEFORE the counter is written: a wrong-key run that burns a
# version and publishes nothing is pure loss.
reset_node
seed_network 30000384 "build 384"
USE_KEY="$OTHER_KEY" run 30000385 "build 386"
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the mismatch" "does not match"

echo "--- 16: --sign-only warns LOUDEST that nothing will check the version"
# --sign-only is the one path with no read-back, and it still burns a version
# and hands back an artifact `fdev network publish` will take by hand. The
# unverified-network warning matters more there, not less.
reset_node
seed_network 30000384 "build 384"
echo "timeout" > "$SANDBOX/node/get_modes"
RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" RIVER_WC_ALLOW_UNVERIFIED=1 \
    RIVER_WC_PREFLIGHT_DELAY=0 \
    ./scripts/publish-web-container.sh --sign-only 2>&1)"
RUN_RC=$?
check "exits 0"                      "0"        "$RUN_RC"
contains "warns that nothing reads back" "does NOT read back"

# ---------------------------------------------------- self-consistency checks

echo "--- 17: published-contract/ that disagrees with itself refuses"
# The id is DERIVED from (wasm, parameters). A stale wasm publishes to one
# address while every check here measures another — a green verdict for a
# publish nobody can find. Distinct from the wrong-key check in 15: that one
# catches a key that is not this contract's, this one a wasm that is not.
reset_node
seed_network 30000384 "build 384"
printf 'a DIFFERENT wasm' > "$SANDBOX/published-contract/web_container_contract.wasm"
run 30000385 "build 386"
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the inconsistency" "disagrees with itself"
printf 'not a real wasm' > "$SANDBOX/published-contract/web_container_contract.wasm"
git_sync
check "the fixture is restored" "$CONTRACT_ID" "$(fdev get-contract-id \
    --code "$SANDBOX/published-contract/web_container_contract.wasm" \
    --parameters "$SANDBOX/published-contract/webapp.parameters")"

echo "--- 18: a counter at the u32 ceiling refuses before writing anything"
# The container's version field is a u32. Left unchecked the run wrote
# counter+1 — 4294967296, a value no publish can ever use — into the version
# file and only then failed, wedging every later publish until someone
# hand-edited it.
reset_node
seed_network 30000384 "build 384"
run 4294967295 "build 386"
not_zero "refuses"
check "the counter file is untouched" "4294967295" "$(counter_now)"
check "nothing was published"         "30000384"   "$(network_version_now)"
contains "names the ceiling" "u32::MAX"

echo "--- 19: one below the ceiling still publishes"
# The boundary is a refusal at u32::MAX, not at u32::MAX - 1.
reset_node
seed_network 4294967290 "build old"
run 4294967294 "build 386"
check "exits 0"                       "0"          "$RUN_RC"
check "signs the last usable version" "4294967295" "$(counter_now)"
check "the network took it"           "4294967295" "$(network_version_now)"

echo "--- 20: zero read-back attempts refuses instead of reporting UNKNOWN"
# With no read-back the script cannot tell a publish that landed from one that
# did not, and used to report UNKNOWN about a publish plainly on the network —
# the stale-read-back failure reached from the configuration side. A wedged
# config has to be loud.
reset_node
seed_network 30000384 "build 384"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
git_sync
RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" \
    RIVER_WC_READBACK_ATTEMPTS=0 RIVER_WC_PREFLIGHT_DELAY=0 \
    ./scripts/publish-web-container.sh 2>&1)"
RUN_RC=$?
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the knob" "RIVER_WC_READBACK_ATTEMPTS"

echo "--- 21: a non-numeric knob refuses rather than falling back to a default"
reset_node
seed_network 30000384 "build 384"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
git_sync
RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" \
    RIVER_WC_READBACK_ATTEMPTS=three RIVER_WC_PREFLIGHT_DELAY=0 \
    ./scripts/publish-web-container.sh 2>&1)"
RUN_RC=$?
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
contains "says it must be a number" "must be a whole number"

# ------------------------------------------------------------ the lock's reach

echo "--- 22: a second publish is refused while the first holds the lock"
# The lock is keyed by contract id and lives OUTSIDE the checkout. River
# development is worktree-based, so a lock inside the repo hands two concurrent
# publishes two different files and serialises nothing. $LOCK_PATH is derived
# in this file from the documented rule, not read out of the script: if the
# lock moves back into the repo, this stops conflicting and the scenario fails.
#
# It also runs with --build, and asserts the build never ran: the artifacts are
# built from INSIDE the lock, so a run that cannot get the lock must not have
# touched the shared target/webapp/webapp.tar.xz on its way to finding out.
reset_node
seed_network 30000384 "build 384"
if command -v flock >/dev/null 2>&1; then
    : > "$SANDBOX/lock.wait"
    rm -f "$SANDBOX/lock.held"
    (
        exec 8>>"$LOCK_PATH"
        flock 8 || exit 1
        : > "$SANDBOX/lock.held"
        while [ -e "$SANDBOX/lock.wait" ]; do sleep 0.05; done
    ) &
    HOLDER=$!
    waited=0
    while [ ! -e "$SANDBOX/lock.held" ] && [ "$waited" -lt 200 ]; do
        sleep 0.05; waited=$((waited + 1))
    done
    if [ ! -e "$SANDBOX/lock.held" ]; then setup_failed "the lock holder never took $LOCK_PATH"; fi
    check "the holder took the lock" "yes" "$([ -e "$SANDBOX/lock.held" ] && echo yes || echo no)"
    run 30000385 "build 386" --build
    not_zero "the second run refuses"
    contains "names the conflict" "already running"
    check "it signed nothing"        "30000385" "$(counter_now)"
    check "it published nothing"     "30000384" "$(network_version_now)"
    check "it did not build either"  ""         "$(cat "$SANDBOX/node/cargo.log")"
    rm -f "$SANDBOX/lock.wait"
    wait "$HOLDER"
else
    echo "    (flock not available — lock scenario skipped)"
fi

echo "--- 23: --build builds from inside the lock"
reset_node
seed_network 30000384 "build 384"
run 30000385 "build 386" --build
check "exits 0"                      "0"        "$RUN_RC"
check "the network took it"          "30000386" "$(network_version_now)"
case "$(cat "$SANDBOX/node/cargo.log")" in
    *"web-container-build-inputs"*) check "it ran the build task" yes yes ;;
    *)                              check "it ran the build task" yes no  ;;
esac
case "$(cat "$SANDBOX/node/cargo.log")" in
    *"--env BUILD_PROFILE="*) check "it pins the build profile explicitly" yes yes ;;
    *)                        check "it pins the build profile explicitly" yes no  ;;
esac

echo "--- 24: the make tasks run NOTHING before the publish script"
# Structural, not behavioural: cargo-make runs a task's `dependencies` to
# completion BEFORE its script, so anything that builds the shared archive from
# a `dependencies` list — or from a line in the script body ahead of the
# publish script — runs before the script exists to lock anything. There is no
# way to observe that from inside the sandbox, so this reads the real Makefile.
#
# The earlier version of this pin asked "is there no `dependencies` key, and
# does the token `--build` appear somewhere in the block". Both were true of a
# block that had simply moved `cargo make web-container-build-inputs` into the
# script body, so the bug came back green. It now scrapes the script BODY, with
# comments and blank lines stripped, and requires that body to be exactly the
# one exec line: anything else in it runs outside the lock.
task_block() {
    awk -v t="[tasks.$1]" '$0==t{f=1;next} /^\[tasks\./{f=0} f' "$REPO_ROOT/Makefile.toml"
}
task_script_body() {
    awk -v t="[tasks.$1]" -v q="'''" '
        $0==t {f=1; next}
        /^\[tasks\./ {f=0}
        f && $0 ~ /^script = / {s=1; next}
        f && s && $0==q {s=0; next}
        f && s {print}
    ' "$REPO_ROOT/Makefile.toml" | grep -v '^[[:space:]]*#' | grep -v '^[[:space:]]*$'
}
for task in publish-river publish-river-debug sign-webapp; do
    blk="$(task_block "$task")"
    if [ -z "$blk" ]; then
        setup_failed "could not find [tasks.$task] in $REPO_ROOT/Makefile.toml"
        continue
    fi
    if printf '%s\n' "$blk" | grep -q '^dependencies'; then
        check "[tasks.$task] has no dependencies list" yes no
    else
        check "[tasks.$task] has no dependencies list" yes yes
    fi
    body="$(task_script_body "$task")"
    if [ -z "$body" ]; then
        setup_failed "could not read the script body of [tasks.$task]"
        continue
    fi
    # Exactly one command, and it is the publish script. A second line — a
    # `cargo make`, an `npm run`, anything — is a build outside the lock.
    check "[tasks.$task] body is a single command" "1" "$(printf '%s\n' "$body" | wc -l)"
    case "$body" in
        *"./scripts/publish-web-container.sh"*)
            check "[tasks.$task] body is the publish script" yes yes ;;
        *)  check "[tasks.$task] body is the publish script" yes no  ;;
    esac
    case "$body" in
        *"--build"*) check "[tasks.$task] passes --build" yes yes ;;
        *)           check "[tasks.$task] passes --build" yes no  ;;
    esac
    # Belt and braces: name the specific reintroduction, so the failure says
    # what went wrong rather than only that a line count changed.
    if printf '%s\n' "$body" | grep -q 'cargo make'; then
        check "[tasks.$task] body does not invoke cargo make" yes no
    else
        check "[tasks.$task] body does not invoke cargo make" yes yes
    fi
done
if printf '%s\n' "$(task_block web-container-build-inputs)" | grep -q '^dependencies'; then
    check "[tasks.web-container-build-inputs] carries the dependencies" yes yes
else
    check "[tasks.web-container-build-inputs] carries the dependencies" yes no
fi

echo "--- 24b: a build that modifies tracked files is caught after the fact"
# [0] vouches for a clean tracked tree and the build runs after it, so the
# re-check on that seam is new code guarding two new features against each
# other. The stub cargo can now dirty a tracked file on demand, which is the
# only way to reach it.
reset_node
seed_network 30000384 "build 384"
echo "1" > "$SANDBOX/node/cargo_dirties_tree"
run 30000385 "build 386" --build
not_zero "refuses"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the seam" "the build modified tracked files"
echo "0" > "$SANDBOX/node/cargo_dirties_tree"
printf 'ui source\n' > "$SANDBOX/ui-source.txt"
git_sync

# ------------------------------------------------------- provenance scenarios
#
# The web container is ONE key serving every River user, and river's main has
# no branch protection, so this script is the only thing enforcing "publish
# from a green main". Each of these is a way that claim can be false.

echo "--- 25: a modified tracked file refuses"
# A tracked file other than the counter, left uncommitted: the checkout differs
# from the commit whose CI the gate just verified, which is the whole
# provenance claim. Deliberately NOT the counter — that is the one file the
# script writes by design, and scenario 33 covers it.
reset_node
seed_network 30000384 "build 384"
printf 'an uncommitted edit\n' >> "$SANDBOX/ui-source.txt"
RUN_SYNC=0 run 30000385 "build 386"
not_zero "refuses"
check "the counter is not advanced"  "30000385" "$(counter_now)"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the dirty tree" "tracked files are modified"
printf 'ui source\n' > "$SANDBOX/ui-source.txt"
git_sync

echo "--- 26: a branch that is not main refuses"
reset_node
seed_network 30000384 "build 384"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
git_sync
git -C "$SANDBOX" checkout -q -b not-main
# RUN_SYNC=0: the tree is already clean and committed, and syncing from a
# branch that is not main is what the git_sync postcondition check catches.
RUN_SYNC=0 run 30000385 "build 386"
not_zero "refuses"
check "the counter is not advanced"  "30000385" "$(counter_now)"
contains "names the branch" "on branch 'not-main'"
git -C "$SANDBOX" checkout -q main
git -C "$SANDBOX" branch -q -D not-main

echo "--- 27: a HEAD that is ahead of origin/main refuses"
reset_node
seed_network 30000384 "build 384"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
git_sync
git -C "$SANDBOX" commit -q --allow-empty -m "unpushed"   # local only
RUN_SYNC=0 run 30000385 "build 386"
not_zero "refuses"
check "the counter is not advanced"  "30000385" "$(counter_now)"
contains "names the divergence" "!= origin/main"
git_sync

echo "--- 28: NO CI runs is not the same as green"
# An empty list yields zero FAILURES, so a guard that only counts bad runs
# reports a commit CI never ran on as green — a guard that cannot fail.
reset_node
seed_network 30000384 "build 384"
printf '%s' '[]' > "$SANDBOX/node/ci_runs"
run 30000385 "build 386"
not_zero "refuses"
check "the counter is not advanced"  "30000385" "$(counter_now)"
contains "says there are no runs" "NO CI runs exist"

echo "--- 29: red CI refuses"
reset_node
seed_network 30000384 "build 384"
printf '%s' '[{"name":"build","status":"completed","conclusion":"failure"}]' \
    > "$SANDBOX/node/ci_runs"
run 30000385 "build 386"
not_zero "refuses"
check "the counter is not advanced"  "30000385" "$(counter_now)"
contains "says CI is not green" "are not green"

echo "--- 30: pending CI is not green either"
reset_node
seed_network 30000384 "build 384"
printf '%s' '[{"name":"build","status":"in_progress","conclusion":null}]' \
    > "$SANDBOX/node/ci_runs"
run 30000385 "build 386"
not_zero "refuses"
contains "says CI is not green" "are not green"

echo "--- 31: the documented escape proceeds, loudly"
reset_node
seed_network 30000384 "build 384"
printf '%s' '[]' > "$SANDBOX/node/ci_runs"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
# Deliberately NOT synced: dirty tree, no CI runs, and it still goes through.
RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" RIVER_WC_ALLOW_UNPROVEN=1 \
    RIVER_WC_READBACK_ATTEMPTS=1 RIVER_WC_READBACK_DELAY=0 RIVER_WC_PREFLIGHT_DELAY=0 \
    ./scripts/publish-web-container.sh 2>&1)"
RUN_RC=$?
check "exits 0"                      "0"        "$RUN_RC"
check "the network took it"          "30000386" "$(network_version_now)"
contains "says the gate was skipped" "provenance gate SKIPPED"

echo "--- 32: invoked from inside scripts/, not from the repo root"
# `cd scripts && ./publish-web-container.sh` used to die with
# `flock: failed to execute` and an exit code outside the documented set,
# because the lock re-exec handed flock "$0" after the script had already cd'd
# to the repo root. The lock is now an fd this process holds, so there is
# nothing to re-exec — this pins that.
reset_node
seed_network 30000384 "build 384"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
git_sync
RUN_OUT="$(cd "$SANDBOX/scripts" && RIVER_WC_KEY_FILE="$KEY_FILE" \
    RIVER_WC_PREFLIGHT_DELAY=0 ./publish-web-container.sh --dry-run 2>&1)"
RUN_RC=$?
check "exits 0"                      "0"        "$RUN_RC"
check "the counter is untouched"     "30000385" "$(counter_now)"
contains "reached the version step" "signing version 30000386"

echo "--- 33: the re-run every failed verdict recommends is ALLOWED"
# THE ONE THAT MATTERS. Every non-landed verdict tells the operator to re-run,
# and the counter it just burned is a TRACKED file — so a gate that refuses any
# dirty tracked file refuses the recovery it recommends. The operator's two ways
# out were `git checkout` on the counter, which IS the 2026-08-04 rollback, and
# an override that drops all four provenance checks at once. Neither is
# acceptable, so a counter that is strictly AHEAD of HEAD is exempt: it is
# provably a prior burn, and it cannot change what is published because the
# version is re-floored against the network anyway.
reset_node
seed_network 30000384 "build 384"
echo "0" > "$SANDBOX/node/publish_lands"   # a publish that does not land
run 30000385 "build 386"
not_zero "the first run reports a failure"
check "it burned a version"          "30000386" "$(counter_now)"
# ...and now the re-run, WITHOUT committing the counter first, exactly as an
# operator following the verdict text would do it.
echo "1" > "$SANDBOX/node/publish_lands"
RUN_SYNC=0 run 30000386 "build 386"
check "the re-run is allowed"        "0"        "$RUN_RC"
check "it signs ABOVE the burned version" "30000387" "$(counter_now)"
check "the network took it"          "30000387" "$(network_version_now)"
contains "warns against git checkout" "Never"
git_sync

echo "--- 34: a counter BELOW HEAD still refuses"
# The exemption is one-directional on purpose. A working-tree counter that is
# not ahead of HEAD means something rolled it back, and a rollback is the
# mechanism of the incident — so that direction stays a hard refusal.
reset_node
seed_network 30000384 "build 384"
RUN_SYNC=0 run 30000001 "build 386"
not_zero "refuses"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the rollback" "not above HEAD's"
git_sync

echo "--- 35: an untracked file under ui/ refuses"
# The gate used to state that untracked files "cannot change what is
# published". They can: tailwind globs ui/src/**/*.rs for class names and dx
# walks ui/assets, so an untracked file under ui/ reaches the archive without
# reaching git — the published site would differ from the artifact CI tested.
reset_node
seed_network 30000384 "build 384"
mkdir -p "$SANDBOX/ui/src"
printf 'fn scratch() {}\n' > "$SANDBOX/ui/src/scratch.rs"
run 30000385 "build 386"
not_zero "refuses"
check "the counter is untouched"     "30000385" "$(counter_now)"
check "nothing was published"        "30000384" "$(network_version_now)"
contains "names the build inputs" "untracked files under ui/"
rm -rf "$SANDBOX/ui"

echo "--- 36: an untracked file outside the build inputs is only a note"
# The refusal is scoped to what was actually checked. Everything else stays a
# note — and the note no longer claims those files cannot change the archive,
# only that they are not under the paths this gate examines.
reset_node
seed_network 30000384 "build 384"
printf 'scratch\n' > "$SANDBOX/scratch-note.txt"
run 30000385 "build 386"
check "exits 0"                      "0"        "$RUN_RC"
check "the network took it"          "30000386" "$(network_version_now)"
contains "notes it without claiming safety" "untracked files present, none under ui/"
rm -f "$SANDBOX/scratch-note.txt"

echo ""
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
