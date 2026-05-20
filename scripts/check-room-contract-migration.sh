#!/bin/bash
# Check if the committed room-contract WASM changed and a migration entry is needed.
#
# The committed WASM at `ui/public/contracts/room_contract.wasm` is what ships
# to users — the UI embeds it via `include_bytes!` and the CLI bundles a synced
# copy. Any change to it moves the room contract key (BLAKE3(wasm, params)) for
# every owner and strands every room on the old key — UNLESS the OLD code hash
# is recorded in `common/legacy_room_contracts.toml`, which lets clients probe
# older generations to recover a dormant room (freenet/river#292).
#
# Usage:
#   scripts/check-room-contract-migration.sh                         # HEAD vs working tree
#   scripts/check-room-contract-migration.sh --ci BASE_SHA HEAD_SHA  # CI mode: two commits
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="common/legacy_room_contracts.toml"
WASM_PATH="ui/public/contracts/room_contract.wasm"

cd "$REPO_ROOT"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"

# Extract all code_hash values from TOML content on stdin.
known_hashes_from_stdin() {
    grep '^code_hash' | sed 's/.*= *"\([^"]*\)".*/\1/'
}

# BLAKE3-hash the committed WASM at a given git ref. Outputs "" if missing.
wasm_hash_at_ref() {
    local ref="$1"
    if git cat-file -e "$ref:$WASM_PATH" 2>/dev/null; then
        git show "$ref:$WASM_PATH" | b3sum | cut -d' ' -f1
    else
        echo ""
    fi
}

wasm_hash_file() {
    b3sum "$1" | cut -d' ' -f1
}

if [ "${1:-}" = "--ci" ]; then
    BASE_SHA="${2:?Usage: check-room-contract-migration.sh --ci BASE_SHA HEAD_SHA}"
    HEAD_SHA="${3:?Usage: check-room-contract-migration.sh --ci BASE_SHA HEAD_SHA}"

    BASE_HASH=$(wasm_hash_at_ref "$BASE_SHA")
    HEAD_HASH=$(wasm_hash_at_ref "$HEAD_SHA")

    if [ -z "$BASE_HASH" ]; then
        echo "Committed room-contract WASM did not exist at base ($BASE_SHA) — treating as new file, no migration needed."
        exit 0
    fi
    if [ -z "$HEAD_HASH" ]; then
        die "Committed room-contract WASM ($WASM_PATH) was deleted at head ($HEAD_SHA). Refusing to proceed."
    fi

    echo "Room-contract WASM hash on base ($BASE_SHA): $BASE_HASH"
    echo "Room-contract WASM hash on head ($HEAD_SHA): $HEAD_HASH"

    if [ "$BASE_HASH" = "$HEAD_HASH" ]; then
        echo "Room-contract WASM unchanged — no migration entry needed."
        exit 0
    fi

    echo ""
    echo "Room-contract WASM CHANGED: $BASE_HASH -> $HEAD_HASH"
    echo ""

    # The head branch's registry must contain the BASE (old) hash.
    if git show "$HEAD_SHA:$TOML_PATH" 2>/dev/null | known_hashes_from_stdin | grep -qF "$BASE_HASH"; then
        echo "Old hash found in $TOML_PATH — migration entry exists."
        exit 0
    fi

    echo "FAILED: Old room-contract hash $BASE_HASH is NOT in $TOML_PATH on HEAD!"
    echo ""
    echo "When the committed room-contract WASM changes, the OLD hash MUST be added to"
    echo "$TOML_PATH so clients can recover rooms stranded on the old contract key."
    echo ""
    echo "To fix:"
    echo "  1. Revert the WASM change, OR"
    echo "  2. Run: cargo make add-room-contract-migration  (before committing the new WASM)"
    exit 1
else
    # Local mode: compare HEAD's committed WASM to the working-tree file.
    [ -f "$WASM_PATH" ] || die "Committed room-contract WASM not found: $WASM_PATH"

    WORKING_HASH=$(wasm_hash_file "$WASM_PATH")
    HEAD_HASH=$(wasm_hash_at_ref "HEAD")

    if [ -z "$HEAD_HASH" ]; then
        echo "Committed room-contract WASM not tracked at HEAD — no migration needed (new file)."
        exit 0
    fi

    echo "Room-contract WASM hash at HEAD:    $HEAD_HASH"
    echo "Room-contract WASM hash in working: $WORKING_HASH"

    if [ "$HEAD_HASH" = "$WORKING_HASH" ]; then
        echo "Committed room-contract WASM matches HEAD — no migration entry needed."
        exit 0
    fi

    echo ""
    echo "Committed room-contract WASM was modified in working tree: $HEAD_HASH -> $WORKING_HASH"
    echo ""

    if grep '^code_hash' "$TOML_PATH" | grep -qF "$HEAD_HASH"; then
        echo "Old hash found in $TOML_PATH — migration entry exists."
        exit 0
    fi

    echo "Old hash NOT in $TOML_PATH!"
    echo ""
    echo "To fix:"
    echo "  1. Revert the WASM change: git checkout HEAD -- $WASM_PATH"
    echo "  2. OR run: cargo make add-room-contract-migration  (before committing the new WASM)"
    exit 1
fi
