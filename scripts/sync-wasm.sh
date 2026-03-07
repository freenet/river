#!/bin/bash
# Build all WASMs and copy to committed locations.
#
# Usage: scripts/sync-wasm.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

echo "Building room-contract and chat-delegate WASMs..."
cargo build --release --target wasm32-unknown-unknown -p room-contract -p chat-delegate --target-dir target

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
