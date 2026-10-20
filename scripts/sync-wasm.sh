#!/bin/bash
# Build all WASMs and copy to committed locations.
#
# Usage: scripts/sync-wasm.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# The committed WASMs must be built with the toolchain rust-toolchain.toml
# pins: a different rustc gives different bytes, so a different contract and
# delegate key. rustup picks the toolchain from the directory cargo is started
# in, and exports RUSTUP_TOOLCHAIN to everything it starts, so `cargo make
# --cwd <repo> sync-wasm` run from outside the repo builds with that
# directory's toolchain even after the `cd` above. Refuse rather than build
# with the wrong one.
PINNED=$(sed -n 's/^channel *= *"\(.*\)".*/\1/p' rust-toolchain.toml)
[ -n "$PINNED" ] || { echo "ERROR: no channel in rust-toolchain.toml"; exit 1; }
if [ -n "${RUSTUP_TOOLCHAIN:-}" ] && [ "${RUSTUP_TOOLCHAIN%%-*}" != "$PINNED" ]; then
    echo "ERROR: RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN overrides the pinned toolchain $PINNED (rust-toolchain.toml)."
    echo "       Run from inside the repo, e.g.: env -C $REPO_ROOT cargo make sync-wasm"
    exit 1
fi
USING=$(rustc --version | awk '{print $2}')
if [ "$USING" != "$PINNED" ]; then
    echo "ERROR: rustc $USING would build the WASMs, but rust-toolchain.toml pins $PINNED."
    echo "       Run from inside the repo, e.g.: env -C $REPO_ROOT cargo make sync-wasm"
    exit 1
fi

echo "Building room-contract and chat-delegate WASMs with rustc $USING..."
# `--locked` is critical: if Cargo.lock drifts on a release machine, the
# produced WASM bytes (and therefore the delegate / contract key) drift
# silently. Anyone running this script must be operating from the
# committed Cargo.lock — that's the only way the rebuilt WASM matches
# what CI verified.
cargo build --locked --release --target wasm32-unknown-unknown -p room-contract -p chat-delegate --target-dir target

SRC_CONTRACT="target/wasm32-unknown-unknown/release/room_contract.wasm"
SRC_DELEGATE="target/wasm32-unknown-unknown/release/chat_delegate.wasm"

copies=(
    "$SRC_CONTRACT:ui/public/contracts/room_contract.wasm"
    "$SRC_CONTRACT:cli/contracts/room_contract.wasm"
    "$SRC_DELEGATE:ui/public/contracts/chat_delegate.wasm"
)

for pair in "${copies[@]}"; do
    src="${pair%%:*}"
    dst="${pair##*:}"
    if [ ! -f "$src" ]; then
        echo "ERROR: Build output not found: $src"
        exit 1
    fi
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    echo "  Copied $(basename "$src") -> $dst"
done

echo ""
echo "All WASMs synced. Verify with:"
echo "  b3sum ui/public/contracts/*.wasm cli/contracts/*.wasm"
