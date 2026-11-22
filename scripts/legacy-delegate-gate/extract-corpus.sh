#!/bin/bash
# Extract every historical chat-delegate WASM from git history into OUT_DIR,
# named <blake3>.wasm, keeping only blobs whose BLAKE3 matches a code_hash in
# legacy_delegates.toml (plus the current committed build). Prints
# "<blake3> <version>" lines to OUT_DIR/index.txt. Needs full history and b3sum.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:?usage: extract-corpus.sh OUT_DIR}"
WASM_PATH="ui/public/contracts/chat_delegate.wasm"
cd "$REPO_ROOT"
command -v b3sum >/dev/null || { echo "ERROR: b3sum not found (cargo install b3sum)" >&2; exit 2; }
if [ "$(git rev-parse --is-shallow-repository)" = true ]; then
  echo "ERROR: shallow clone; history needed (git fetch --unshallow / fetch-depth: 0)" >&2; exit 2
fi
mkdir -p "$OUT"; : > "$OUT/index.txt"
declare -A want
ver=""
while IFS= read -r line; do
  case "$line" in
    version*) ver=$(sed 's/.*"\(.*\)".*/\1/' <<<"$line");;
    code_hash*) want[$(sed 's/.*"\(.*\)".*/\1/' <<<"$line")]=$ver;;
  esac
done < legacy_delegates.toml
declare -A seen
for c in $(git log --format=%H -- "$WASM_PATH"); do
  blob=$(git rev-parse "$c:$WASM_PATH" 2>/dev/null) || continue
  [ -n "${seen[$blob]:-}" ] && continue
  seen[$blob]=1
  h=$(git cat-file blob "$blob" | b3sum | cut -d' ' -f1)
  if [ -n "${want[$h]:-}" ]; then
    git cat-file blob "$blob" > "$OUT/$h.wasm"
    echo "$h ${want[$h]}" >> "$OUT/index.txt"
  fi
done
git cat-file blob "HEAD:$WASM_PATH" > "$OUT/current.wasm.tmp"
h=$(b3sum "$OUT/current.wasm.tmp" | cut -d' ' -f1)
mv "$OUT/current.wasm.tmp" "$OUT/$h.wasm"; echo "$h CURRENT" >> "$OUT/index.txt"
sort -u -o "$OUT/index.txt" "$OUT/index.txt"
