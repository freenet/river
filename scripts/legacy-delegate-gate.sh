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
#      CARGO_TARGET_DIR (reuse a build cache), GATE_KEEP=1 (keep the core copy).
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
case "$WORK" in "$HOME/code/"*) ;; *) die "GATE_WORK must be under ~/code (build hook)";; esac

mkdir -p "$WORK"
CORPUS="$WORK/corpus"; CORE="$WORK/core"
rm -rf "$CORPUS" "$CORE"
"$GATE/extract-corpus.sh" "$CORPUS" || die "corpus extraction failed"

# Resolve manifest versions to hashes.
RESOLVED="$WORK/resolved.txt"; : > "$RESOLVED"
min=0
while read -r ver expect enc rest; do
  case "$ver" in ''|'#'*) continue;; esac
  h=$(awk -v v="$ver" '$2==v{print $1}' "$CORPUS/index.txt")
  [ -n "$h" ] || die "manifest names $ver but no WASM with a registry hash was found in git history"
  echo "$h $ver $expect $enc" >> "$RESOLVED"
  [ "$expect" = works ] && min=$((min+1))
done < "$GATE/manifest.txt"
echo "corpus: $(wc -l < "$RESOLVED") entries ($min expected working)"

# Fetch the core version under test.
if [ -n "$CORE_PATH" ]; then
  [ -f "$CORE_PATH/crates/core/src/wasm_runtime/delegate/test.rs" ] || die "$CORE_PATH is not a freenet-core checkout"
  # Committed HEAD only (git archive): excludes target dirs and nested worktrees.
  mkdir -p "$CORE"
  git -C "$CORE_PATH" archive HEAD | tar -C "$CORE" -xf -
else
  git init -q "$CORE"
  git -C "$CORE" fetch -q --depth 1 https://github.com/freenet/freenet-core "$CORE_REV" || die "cannot fetch freenet-core $CORE_REV"
  git -C "$CORE" checkout -q FETCH_HEAD
fi
echo "core: $(grep -m1 '^version' "$CORE/crates/core/Cargo.toml") ${CORE_REV:-$CORE_PATH}"

# Inject the harness as a child module of the core delegate test module.
T="$CORE/crates/core/src/wasm_runtime/delegate"
cp "$GATE/legacy_gate.rs" "$T/legacy_gate.rs"
printf '\n#[path = "legacy_gate.rs"]\nmod legacy_gate;\n' >> "$T/test.rs"

cd "$CORE"
export LEGACY_GATE_CORPUS="$CORPUS" LEGACY_GATE_MANIFEST="$RESOLVED" LEGACY_GATE_MIN_WORKING="$min"
LOG="$WORK/run.log"
set +e
cargo test -p freenet --lib -- legacy_gate:: --nocapture --test-threads=1 > "$LOG" 2>&1
rc=$?
set -e
grep -E 'LEGACY-GATE|test result|legacy delegate gate failed|^  V|^  CURRENT|panicked' "$LOG" || true
if [ $rc -ne 0 ]; then
  if grep -q 'LEGACY-GATE summary' "$LOG" || grep -q 'panicked' "$LOG"; then
    echo "GATE FAILED (log: $LOG)"; exit 1
  fi
  echo "SETUP ERROR: harness did not compile/run against this core (log: $LOG)" >&2; tail -30 "$LOG" >&2; exit 2
fi
grep -q 'test result: ok. 2 passed' "$LOG" || die "expected exactly the 2 gate tests to run; got something else (log: $LOG)"
echo "GATE PASSED"
