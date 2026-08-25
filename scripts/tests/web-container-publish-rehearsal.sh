#!/usr/bin/env bash
# Rehearse scripts/publish-web-container.sh end to end against a STUB node.
#
# The decision logic is unit-tested in web-container-publish-lib-test.sh. This
# covers the wiring around it, which is where a publish script actually breaks:
# the fdev argument order, the `Contract not found` string it keys absence off,
# the `version=` line it parses, whether the counter really is forward-only,
# and whether the verdict comes out of the read-back rather than the exit code.
# None of that is reachable from a pure function, and none of it can be
# exercised against the real network (the web container's key is shared and
# publishing to it from a test is out of bounds).
#
# So: a throwaway repo layout, a throwaway signing key, and an `fdev` on PATH
# that is a shell script. The real publish script and the real
# web-container-tool run unmodified.
#
# Scenario 2 is the 2026-08-04 incident (River commit 1032d373) reproduced:
# a publish that reports a timeout and lands anyway.
#
# Run: ./scripts/tests/web-container-publish-rehearsal.sh
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not available"; exit 0; }

# ------------------------------------------------------------------- the tool
TOOL_SRC="${RIVER_WC_TOOL:-}"
if [ -z "$TOOL_SRC" ]; then
    for candidate in \
        "$REPO_ROOT/target/native/x86_64-unknown-linux-gnu/release/web-container-tool" \
        "$REPO_ROOT/target/native/x86_64-unknown-linux-gnu/debug/web-container-tool" \
        "$REPO_ROOT/target/release/web-container-tool" \
        "$REPO_ROOT/target/debug/web-container-tool"
    do
        [ -x "$candidate" ] && { TOOL_SRC="$candidate"; break; }
    done
fi
if [ -z "$TOOL_SRC" ]; then
    echo "building web-container-tool..."
    (cd "$REPO_ROOT" && cargo build -p web-container-tool) || {
        echo "FAIL: could not build web-container-tool"; exit 1; }
    TOOL_SRC="$REPO_ROOT/target/debug/web-container-tool"
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

# ----------------------------------------------------------- throwaway key
# Never the production key at ~/.config/river/web-container-keys.toml. The
# publish script's parameters check would refuse it here anyway, since the
# sandbox's committed parameters are this key's.
KEY_FILE="$SANDBOX/keys.toml"
"$TOOL" generate --output "$KEY_FILE" >/dev/null
"$TOOL" export-parameters --parameters "$SANDBOX/published-contract/webapp.parameters" \
    --key-file "$KEY_FILE" >/dev/null
echo "TESTCONTRACTKEYnotarealbase58key" > "$SANDBOX/published-contract/contract-id.txt"
printf 'not a real wasm' > "$SANDBOX/published-contract/web_container_contract.wasm"

# ----------------------------------------------------------------- stub fdev
# Behaviour is driven entirely by files under $SANDBOX/node:
#   state.bin     the packed state the "network" holds (absent = not found)
#   get_modes     whitespace-separated queue of ok | notfound | timeout, one
#                 consumed per GET, the last one repeating — so a pre-flight
#                 read and a post-publish read can behave differently
#   publish_rc    exit code the publish reports
#   publish_lands 1 = the publish updates state.bin, 0 = it is a no-op
cat > "$SANDBOX/bin/fdev" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
NODE="${STUB_NODE_DIR:?}"
args=("$@")
# Skip the MODE positional if present.
[ "${args[0]:-}" = "network" ] && args=("${args[@]:1}")

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
      notfound) echo "Error: Contract not found: TESTCONTRACTKEYnotarealbase58key" >&2; exit 1 ;;
      timeout)  echo "Error: operation timed out after 180s" >&2; exit 1 ;;
    esac
    if [ ! -s "$NODE/state.bin" ]; then
      echo "Error: Contract not found: TESTCONTRACTKEYnotarealbase58key" >&2; exit 1
    fi
    cp "$NODE/state.bin" "$out"
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
    if [ -s "$NODE/collision.bin" ]; then
      # Someone else's state, already at the version we are about to publish.
      # Only our own key can sign one the contract accepts, so in reality this
      # is an earlier run of this pipeline — the 2026-08-04 fork.
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
      # `version <= current_version` — and does so as a NO-OP SUCCESS, not an
      # error. Modelling that here is the point: it is what makes
      # "fdev exited 0" mean nothing.
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
export STUB_NODE_DIR="$SANDBOX/node"
export STUB_TOOL="$TOOL"
export PATH="$SANDBOX/bin:$PATH"

# ------------------------------------------------------------------- helpers
PASS=0
FAIL=0
check() {
    if [ "$2" = "$3" ]; then PASS=$((PASS + 1)); else
        FAIL=$((FAIL + 1))
        echo "FAIL: $1"
        echo "      expected: $2"
        echo "      actual:   $3"
    fi
}

# Put a signed state at $1 carrying archive bytes $2 onto the stub network.
seed_network() {
    local version="$1" body="$2"
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
}

# run <counter> <archive-body> [args...] -> sets RUN_RC / RUN_OUT
run() {
    local counter="$1" body="$2"; shift 2
    echo "$counter" > "$SANDBOX/published-contract/contract-version.txt"
    printf '%s' "$body" > "$SANDBOX/target/webapp/webapp.tar.xz"
    RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" \
        RIVER_WC_READBACK_ATTEMPTS=1 RIVER_WC_READBACK_DELAY=0 \
        ./scripts/publish-web-container.sh "$@" 2>&1)"
    RUN_RC=$?
}
counter_now() { tr -d '[:space:]' < "$SANDBOX/published-contract/contract-version.txt"; }
network_version_now() {
    "$TOOL" inspect --state "$SANDBOX/node/state.bin" \
        --parameters "$SANDBOX/published-contract/webapp.parameters" \
        | sed -n 's/^version=//p'
}

reset_node() {
    echo "ok" > "$SANDBOX/node/get_modes"
    echo "0"  > "$SANDBOX/node/publish_rc"
    echo "1"  > "$SANDBOX/node/publish_lands"
    rm -f "$SANDBOX/node/state.bin" "$SANDBOX/node/version" "$SANDBOX/node/collision.bin"
}

# ------------------------------------------------------------------ scenarios

echo "--- 1: an ordinary publish"
reset_node
seed_network 30000384 "build 384"
run 30000385 "build 386"
check "exits 0"                 "0"        "$RUN_RC"
check "signs counter+1"         "30000386" "$(counter_now)"
check "the network took it"     "30000386" "$(network_version_now)"
case "$RUN_OUT" in *"PUBLISHED"*) check "reports PUBLISHED" yes yes ;;
                  *)              check "reports PUBLISHED" yes no  ;; esac

echo "--- 2: the 2026-08-04 incident — a publish that times out and lands anyway"
reset_node
seed_network 30000376 "build 376"
echo "1" > "$SANDBOX/node/publish_rc"     # fdev reports failure...
echo "1" > "$SANDBOX/node/publish_lands"  # ...but the PUT landed
run 30000376 "build A of the UI"
check "exits 0 despite fdev failing"  "0"        "$RUN_RC"
check "the state IS live"             "30000377" "$(network_version_now)"
check "the counter is NOT rolled back" "30000377" "$(counter_now)"
case "$RUN_OUT" in *"despite fdev exiting"*) check "says so out loud" yes yes ;;
                  *)                         check "says so out loud" yes no  ;; esac

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
check "does not exit 0"              "no"       "$([ "$RUN_RC" -eq 0 ] && echo yes || echo no)"
check "the counter is untouched"     "30000385" "$(counter_now)"
case "$RUN_OUT" in *"did not learn"*) check "explains why" yes yes ;;
                  *)                  check "explains why" yes no  ;; esac

echo "--- 5: 'Contract not found' is not proof of absence for THIS contract"
reset_node
echo "notfound" > "$SANDBOX/node/get_modes"
run 30000385 "build 386"
check "refuses"                      "no"       "$([ "$RUN_RC" -eq 0 ] && echo yes || echo no)"
check "the counter is untouched"     "30000385" "$(counter_now)"
case "$RUN_OUT" in *"Contract not found"*) check "names the answer it got" yes yes ;;
                  *)                       check "names the answer it got" yes no  ;; esac

echo "--- 6: the override publishes, and the read-back still judges it"
# Pre-flight read fails, the operator overrides, and the publish is a silent
# no-op that exits 0 — the shape a stale-version publish takes. The read-back
# (which succeeds) is the only thing that can catch it.
reset_node
seed_network 30000384 "build 384"
echo "timeout ok" > "$SANDBOX/node/get_modes"
echo "0" > "$SANDBOX/node/publish_rc"
echo "0" > "$SANDBOX/node/publish_lands"
echo "30000385" > "$SANDBOX/published-contract/contract-version.txt"
printf 'build 386' > "$SANDBOX/target/webapp/webapp.tar.xz"
RUN_OUT="$(cd "$SANDBOX" && RIVER_WC_KEY_FILE="$KEY_FILE" RIVER_WC_ALLOW_UNVERIFIED=1 \
    RIVER_WC_READBACK_ATTEMPTS=1 RIVER_WC_READBACK_DELAY=0 \
    ./scripts/publish-web-container.sh 2>&1)"
RUN_RC=$?
check "does not exit 0 on a no-op publish" "no" "$([ "$RUN_RC" -eq 0 ] && echo yes || echo no)"
check "the counter stays forward"          "30000386" "$(counter_now)"
check "the network never took it"          "30000384" "$(network_version_now)"
case "$RUN_OUT" in *"NOT PUBLISHED"*) check "reports NOT PUBLISHED" yes yes ;;
                  *)                  check "reports NOT PUBLISHED" yes no  ;; esac

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
OTHER_KEY="$SANDBOX/other-keys.toml"
"$TOOL" generate --output "$OTHER_KEY" >/dev/null
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
check "refuses"                      "no"       "$([ "$RUN_RC" -eq 0 ] && echo yes || echo no)"
check "the counter is untouched"     "30000385" "$(counter_now)"
case "$RUN_OUT" in *"do NOT verify"*) check "says the provenance is unclear" yes yes ;;
                  *)                  check "says the provenance is unclear" yes no  ;; esac

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
check "does not exit 0"        "no" "$([ "$RUN_RC" -eq 0 ] && echo yes || echo no)"
case "$RUN_OUT" in *"FORKED"*) check "reports FORKED" yes yes ;;
                  *)           check "reports FORKED" yes no  ;; esac

echo ""
echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
