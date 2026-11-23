#!/bin/bash
# Fail if a chat-delegate generation in legacy_delegates.toml whose WASM exists in
# git history has no line in manifest.txt. Needs full history and b3sum, and runs no
# WASM, so CI can run it on every PR. legacy-delegate-gate.sh runs the same check
# against its own corpus, but only at publish time, which is how V39 (#979) reached
# a publish with no manifest line. Usage: check-manifest-coverage.sh [index.txt]
# (index.txt: "<blake3> <version>" lines from extract-corpus.sh; made here if absent).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
MANIFEST="$HERE/manifest.txt"
INDEX="${1:-}"
if [ -z "$INDEX" ]; then
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  "$HERE/extract-corpus.sh" "$tmp" >/dev/null
  INDEX="$tmp/index.txt"
fi
[ -s "$INDEX" ] || { echo "ERROR: empty corpus index $INDEX" >&2; exit 2; }
untested=$(awk '{print $2}' "$INDEX" | while read -r v; do
  awk -v v="$v" '!/^#/ && $1==v{f=1} END{exit !f}' "$MANIFEST" || echo "$v"; done)
if [ -n "$untested" ]; then
  echo "ERROR: WASM in git history for registry generation(s) not in manifest.txt: $(echo $untested)." >&2
  echo "Add each to scripts/legacy-delegate-gate/manifest.txt as works or dead (run 'cargo make legacy-delegate-gate' to learn which)." >&2
  exit 1
fi
echo "manifest.txt covers every registry generation that has a WASM in git history"
