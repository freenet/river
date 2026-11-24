#!/usr/bin/env bash
# Build one River UI bundle retargeted to the THROWAWAY staging container and
# sign it with the throwaway key. Never touches production keys or counters.
#
#   build-ui.sh <river-worktree> <out-dir> <version>
#
# Needs WCT (web-container-tool binary), KEYS (throwaway key file) and PARAMS
# (its web-container parameters) in the environment; run.sh sets all three.
#
# Writes <out-dir>/webapp.tar.xz, webapp.metadata, chat_delegate.wasm,
# room_contract.wasm, commit.txt. Skips the build when <out-dir>/commit.txt
# already names the worktree's HEAD.
set -euo pipefail
WT=$(readlink -f "$1"); OUT=$(readlink -f -m "$2"); VERSION=$3
: "${WCT:?set WCT}" "${KEYS:?set KEYS}" "${PARAMS:?set PARAMS}"
# Prefer the cargo-installed dx: an older dx earlier on PATH builds a bundle
# that does not match the UI's dioxus version.
export PATH=$HOME/.cargo/bin:$PATH
export RUSTC_WRAPPER=   # dx probes the wrapper with a C file; sccache rejects it
HEAD=$(git -C "$WT" rev-parse HEAD)
mkdir -p "$OUT"
if [ -f "$OUT/commit.txt" ] && [ "$(cat "$OUT/commit.txt")" = "$HEAD" ] && [ -f "$OUT/webapp.tar.xz" ]; then
  echo "ui $HEAD already built in $OUT"; exit 0
fi
TEST_ID=$(fdev get-contract-id --code "$WT/published-contract/web_container_contract.wasm" --parameters "$PARAMS")
echo "building ui at $HEAD for staging container $TEST_ID"
DT=$WT/ui/Dioxus.toml
restore() { git -C "$WT" checkout -- ui/Dioxus.toml; }
trap restore EXIT
perl -pi -e "s|^\\s*#?\\s*base_path\\s*=\\s*\".*\"|base_path = \"v1/contract/web/${TEST_ID}\"|" "$DT"
grep -q "^base_path = \"v1/contract/web/${TEST_ID}\"" "$DT"
[ -d "$WT/ui/node_modules" ] || (cd "$WT/ui" && npm ci --silent)
(cd "$WT/ui" && npm run --silent build:css)
rm -rf "$WT/target/dx/river-ui/release/web/public/assets"
touch "$WT/ui/src/main.rs"
(cd "$WT/ui" && dx build --release)
restore; trap - EXIT
PUB=$WT/target/dx/river-ui/release/web/public
grep -qF "$TEST_ID" "$PUB/index.html" || { echo "bundle does not reference $TEST_ID" >&2; exit 1; }
if grep -rqF raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv "$PUB"; then echo "bundle references the PRODUCTION container" >&2; exit 1; fi
(cd "$PUB" && tar -cJf "$OUT/webapp.tar.xz" *)
"$WCT" sign --input "$OUT/webapp.tar.xz" --output "$OUT/webapp.metadata" --parameters "$PARAMS" \
  --version "$VERSION" --key-file "$KEYS"
cp "$WT/ui/public/contracts/chat_delegate.wasm" "$WT/ui/public/contracts/room_contract.wasm" "$OUT/"
echo "$HEAD" > "$OUT/commit.txt"
echo "ui built: $OUT (delegate $(b3sum --no-names "$OUT/chat_delegate.wasm" | cut -c1-8), room $(b3sum --no-names "$OUT/room_contract.wasm" | cut -c1-8), version $VERSION)"
