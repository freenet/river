#!/bin/bash
# Check if the delegate WASM changed and a migration entry is needed.
#
# Usage:
#   scripts/check-migration.sh          # compare committed vs freshly built
#   scripts/check-migration.sh --ci BASE_SHA HEAD_SHA  # CI mode: compare two commits
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/legacy_delegates.toml"

die() { echo "ERROR: $*" >&2; exit 1; }

# Ensure b3sum is available
command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"

# Extract all code_hash values from the TOML
known_hashes() {
    grep '^code_hash' "$TOML" | sed 's/.*= *"\([^"]*\)".*/\1/'
}

wasm_blake3() {
    b3sum "$1" | cut -d' ' -f1
}

if [ "${1:-}" = "--ci" ]; then
    # CI mode: build WASM on base and head, compare
    BASE_SHA="${2:?Usage: check-migration.sh --ci BASE_SHA HEAD_SHA}"
    HEAD_SHA="${3:?Usage: check-migration.sh --ci BASE_SHA HEAD_SHA}"

    echo "Building delegate WASM on base ($BASE_SHA)..."
    git checkout "$BASE_SHA" --quiet
    git submodule update --init --recursive --quiet
    cargo build --release --target wasm32-unknown-unknown -p chat-delegate --target-dir target 2>/dev/null
    BASE_HASH=$(wasm_blake3 target/wasm32-unknown-unknown/release/chat_delegate.wasm)
    echo "  Base hash: $BASE_HASH"

    # Clean to avoid stale artifacts
    cargo clean -p chat-delegate --release --target wasm32-unknown-unknown 2>/dev/null

    echo "Building delegate WASM on head ($HEAD_SHA)..."
    git checkout "$HEAD_SHA" --quiet
    git submodule update --init --recursive --quiet
    cargo build --release --target wasm32-unknown-unknown -p chat-delegate --target-dir target 2>/dev/null
    HEAD_HASH=$(wasm_blake3 target/wasm32-unknown-unknown/release/chat_delegate.wasm)
    echo "  Head hash: $HEAD_HASH"

    if [ "$BASE_HASH" = "$HEAD_HASH" ]; then
        echo "Delegate WASM unchanged — no migration needed."
        exit 0
    fi

    echo ""
    echo "Delegate WASM CHANGED: $BASE_HASH -> $HEAD_HASH"

    # Check if base hash is in the TOML
    if known_hashes | grep -qF "$BASE_HASH"; then
        echo "Old hash found in legacy_delegates.toml — migration entry exists."
        exit 0
    fi

    echo ""
    echo "FAILED: Old delegate hash $BASE_HASH is NOT in legacy_delegates.toml!"
    echo ""
    echo "When the delegate WASM changes, you MUST add the old hash to legacy_delegates.toml"
    echo "so existing users can migrate their room data to the new delegate."
    echo ""
    echo "Run:  cargo make add-migration"
    exit 1
else
    # Local mode: compare committed WASM vs freshly built
    COMMITTED="$REPO_ROOT/ui/public/contracts/chat_delegate.wasm"
    [ -f "$COMMITTED" ] || die "Committed WASM not found: $COMMITTED"

    COMMITTED_HASH=$(wasm_blake3 "$COMMITTED")
    echo "Committed delegate WASM hash: $COMMITTED_HASH"

    echo "Building delegate WASM..."
    cargo build --release --target wasm32-unknown-unknown -p chat-delegate --target-dir target 2>/dev/null
    BUILT="$REPO_ROOT/target/wasm32-unknown-unknown/release/chat_delegate.wasm"
    BUILT_HASH=$(wasm_blake3 "$BUILT")
    echo "Freshly built delegate WASM hash: $BUILT_HASH"

    if [ "$COMMITTED_HASH" = "$BUILT_HASH" ]; then
        echo "Delegate WASM unchanged — no migration needed."
        exit 0
    fi

    echo ""
    echo "Delegate WASM CHANGED: $COMMITTED_HASH -> $BUILT_HASH"

    if known_hashes | grep -qF "$COMMITTED_HASH"; then
        echo "Old hash found in legacy_delegates.toml — migration entry exists."
    else
        echo ""
        echo "Old hash NOT in legacy_delegates.toml!"
        echo "Run:  cargo make add-migration"
        exit 1
    fi

    # Also check if committed WASM needs updating
    if ! cmp -s "$COMMITTED" "$BUILT"; then
        echo ""
        echo "NOTE: Committed WASM is stale. Run:  cargo make sync-wasm"
    fi
fi
