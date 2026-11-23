#!/bin/bash
# Run River's legacy chat-delegate release gate (river#630) against a freenet-core version.
#
# Usage:
#   scripts/legacy-delegate-gate.sh --core-rev <tag|sha|branch>   # e.g. v0.2.162 or a commit
#   scripts/legacy-delegate-gate.sh --core-path <checkout>        # a local git checkout (its committed HEAD is exported; never modified)
#
# What it does: re-extracts every old chat-delegate WASM from River git history,
# verifies each against legacy_delegates.toml, copies a test harness into a scratch
# copy of freenet-core and runs it on that core's real wasmtime runtime with a real
# file-backed SecretsStore. Exit codes: 0 pass; 1 gate FAILED (a delegate broke or a
# guard tripped); 2 setup error (harness could not run; NOT a pass).
#
# Env: GATE_WORK (scratch dir, default ~/code/scratch/legacy-delegate-gate),
#      CARGO_TARGET_DIR (reuse a build cache). The core checkout is kept per core rev
#      under GATE_WORK so repeat runs reuse the compiled core (unchanged mtimes).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
RIVER="$(cd "$HERE/.." && pwd)"
GATE="$HERE/legacy-delegate-gate"
WORK="${GATE_WORK:-$HOME/code/scratch/legacy-delegate-gate}"
die() { echo "SETUP ERROR: $*" >&2; exit 2; }

CORE_REV=""; CORE_PATH=""
while [ $# -gt 0 ]; do
  case "$1" in
    --core-rev) CORE_REV="${2:?}"; shift 2;;
    --core-path) CORE_PATH="${2:?}"; shift 2;;
    *) die "unknown argument $1 (see header)";;
  esac
done
[ -n "$CORE_REV$CORE_PATH" ] || die "give --core-rev or --core-path"
[ -z "$CORE_REV" ] || [ -z "$CORE_PATH" ] || die "give only one of --core-rev / --core-path"
case "$WORK" in ""|/|"$HOME"|"$HOME/code") die "refusing GATE_WORK=$WORK (the script deletes \$WORK/corpus and \$WORK/core)";; esac

mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
# One run at a time per work dir: concurrent runs would delete each other's core copy.
exec 9>"$WORK/.lock"
flock -w 3600 9 || die "could not take $WORK/.lock within an hour (another gate run is using it)"
CORPUS="$WORK/corpus"
rm -rf "$CORPUS"
"$GATE/extract-corpus.sh" "$CORPUS" || die "corpus extraction failed"

# Resolve manifest versions to hashes.
RESOLVED="$WORK/resolved.txt"; : > "$RESOLVED"
min=0
while read -r ver expect enc rest; do
  case "$ver" in ''|'#'*) continue;; esac
  h=$(awk -v v="$ver" '$2==v{print $1}' "$CORPUS/index.txt")
  errtok=""; case "$rest" in err=*) errtok="${rest%% *}";; esac
  if [ "$expect" = dead ] && [ -z "${errtok#err=}" ]; then die "dead entry $ver needs an err=<token> failure class in manifest.txt"; fi
  [ -n "$h" ] || die "manifest names $ver but no WASM with a registry hash was found in git history"
  echo "$h $ver $expect $enc $errtok" >> "$RESOLVED"
  if [ "$expect" = works ]; then min=$((min+1)); fi
done < "$GATE/manifest.txt"
# Fail-open guards. The floor is hard-coded here, NOT derived from the manifest, so
# emptying the manifest cannot lower it.
MIN_WORKING_FLOOR=10
[ "$min" -ge "$MIN_WORKING_FLOOR" ] || die "manifest expects only $min working generations; floor is $MIN_WORKING_FLOOR"
grep -q ' CURRENT works ' "$RESOLVED" || die "manifest must list CURRENT as works"
# Coverage: every registry generation whose WASM exists in git history must be in the
# manifest, so a newly retired generation cannot be silently left untested.
# (Registry versions with no matching blob in history cannot be run; they are reported.)
untested=$(awk '{print $2}' "$CORPUS/index.txt" | while read -r v; do
  awk -v v="$v" '!/^#/ && $1==v{f=1} END{exit !f}' "$GATE/manifest.txt" || echo "$v"; done)
[ -z "$untested" ] || die "WASM in git history for registry generation(s) not in manifest.txt: $(echo $untested). Add each as works or dead."
nreg=$(grep -c '^version' "$RIVER/legacy_delegates.toml" || true)
echo "corpus: $(wc -l < "$RESOLVED") entries ($min expected working); $((nreg - $(wc -l < "$CORPUS/index.txt") + 1)) registry generations have no WASM in git history and cannot be run"

# Fetch the core version under test into a per-rev checkout that is KEPT, so cargo's
# mtime-based freshness check reuses the compiled core on repeat runs.
CORE_UPSTREAM="https://github.com/freenet/freenet-core"
if [ -n "$CORE_PATH" ]; then
  [ -f "$CORE_PATH/crates/core/src/wasm_runtime/delegate/test.rs" ] || die "$CORE_PATH is not a freenet-core checkout"
  sha=$(git -C "$CORE_PATH" rev-parse HEAD) || die "$CORE_PATH is not a git checkout"
  CORE="$WORK/core-$sha"
  if [ ! -d "$CORE" ]; then
    # Committed HEAD only (git archive): excludes target dirs and nested worktrees.
    rm -rf "$CORE.tmp"; mkdir -p "$CORE.tmp" || die "cannot create $CORE.tmp"
    git -C "$CORE_PATH" archive HEAD | tar -C "$CORE.tmp" -xf - || die "cannot export $CORE_PATH"
    mv "$CORE.tmp" "$CORE" || die "cannot move $CORE.tmp"
  fi
else
  CORE="$WORK/core-$(printf %s "$CORE_REV" | tr -c 'A-Za-z0-9._-' _)"
  [ -d "$CORE/.git" ] || { rm -rf "$CORE"; git init -q "$CORE"; }
  git -C "$CORE" fetch -q --depth 1 "$CORE_UPSTREAM" "$CORE_REV" || die "cannot fetch freenet-core $CORE_REV"
  # Check out only when the commit changed or the tree was altered beyond the harness
  # injection (a forced checkout also reverts the harness, so it is avoided when clean).
  want=$(git -C "$CORE" rev-parse 'FETCH_HEAD^{commit}') || die "cannot resolve $CORE_REV"
  dirty=$(git -C "$CORE" status --porcelain --untracked-files=no | grep -v 'wasm_runtime/delegate/test.rs$' || true)
  if [ "$(git -C "$CORE" rev-parse HEAD 2>/dev/null)" != "$want" ] || [ -n "$dirty" ]; then
    rm -f "$CORE/.git/index.lock"
    git -C "$CORE" checkout -q -f "$want" || die "cannot check out $CORE_REV"
  fi
fi
echo "core: $(grep -m1 '^version' "$CORE/crates/core/Cargo.toml") ${CORE_REV:-$CORE_PATH}"

# Inject the harness as a child module of the core delegate test module.
T="$CORE/crates/core/src/wasm_runtime/delegate"
# Touch files only when their content changes (keeps the compiled core warm).
cmp -s "$GATE/legacy_gate.rs" "$T/legacy_gate.rs" || cp "$GATE/legacy_gate.rs" "$T/legacy_gate.rs"
grep -q '^mod legacy_gate;' "$T/test.rs" || printf '\n#[path = "legacy_gate.rs"]\nmod legacy_gate;\n' >> "$T/test.rs"

cd "$CORE"
export LEGACY_GATE_CORPUS="$CORPUS" LEGACY_GATE_MANIFEST="$RESOLVED" LEGACY_GATE_MIN_WORKING="$min"
LOG="$WORK/run.log"
set +e
cargo test -p freenet --lib -- legacy_gate:: --nocapture --test-threads=1 > "$LOG" 2>&1
rc=$?
set -e
grep -E 'LEGACY-GATE|test result|legacy delegate gate failed|^  V|^  CURRENT|panicked' "$LOG" || true
if [ $rc -ne 0 ]; then
  # Exit 1 only when the gate tests actually ran and reported (the summary line or a
  # FAILED gate test). Anything else (compile error, setup panic) is exit 2.
  if grep -q 'LEGACY-GATE summary' "$LOG" || grep -qE 'legacy_gate::.* \.\.\. FAILED' "$LOG"; then
    echo "GATE FAILED (log: $LOG). If the failures are 'storage:', 'runtime:' or 'store_delegate:' errors rather than 'host error:' or HASH MISMATCH, the harness needs fixing against this core: fix scripts/legacy-delegate-gate/ and re-run. Never skip."; exit 1
  fi
  echo "SETUP ERROR: harness did not compile/run against this core (log: $LOG)" >&2; tail -30 "$LOG" >&2; exit 2
fi
grep -q 'test result: ok. 2 passed' "$LOG" || die "expected exactly the 2 gate tests to run; got something else (log: $LOG)"
echo "GATE PASSED"
