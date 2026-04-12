#!/bin/bash
# Check if the committed delegate WASM changed and a migration entry is needed.
#
# The committed WASM at `ui/public/contracts/chat_delegate.wasm` is what
# actually ships to users — the UI embeds it via `include_bytes!`. Any change
# to this file changes the delegate key and orphans users' rooms_data unless
# a migration entry is added to `legacy_delegates.toml`.
#
# Usage:
#   scripts/check-migration.sh                         # compare HEAD vs working tree
#   scripts/check-migration.sh --ci BASE_SHA HEAD_SHA  # CI mode: compare two commits
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="legacy_delegates.toml"
WASM_PATH="ui/public/contracts/chat_delegate.wasm"

cd "$REPO_ROOT"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"

# Extract all code_hash values from TOML content on stdin
known_hashes_from_stdin() {
    grep '^code_hash' | sed 's/.*= *"\([^"]*\)".*/\1/'
}

# Hash the committed WASM at a given git ref. Outputs "" if missing.
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
    BASE_SHA="${2:?Usage: check-migration.sh --ci BASE_SHA HEAD_SHA}"
    HEAD_SHA="${3:?Usage: check-migration.sh --ci BASE_SHA HEAD_SHA}"

    BASE_HASH=$(wasm_hash_at_ref "$BASE_SHA")
    HEAD_HASH=$(wasm_hash_at_ref "$HEAD_SHA")

    if [ -z "$BASE_HASH" ]; then
        echo "Committed WASM did not exist at base ($BASE_SHA) — treating as new file, no migration needed."
        exit 0
    fi
    if [ -z "$HEAD_HASH" ]; then
        die "Committed WASM ($WASM_PATH) was deleted at head ($HEAD_SHA). Refusing to proceed."
    fi

    echo "Committed WASM hash on base ($BASE_SHA): $BASE_HASH"
    echo "Committed WASM hash on head ($HEAD_SHA): $HEAD_HASH"

    if [ "$BASE_HASH" = "$HEAD_HASH" ]; then
        echo "Committed WASM unchanged — no migration needed."
        exit 0
    fi

    echo ""
    echo "Committed WASM CHANGED: $BASE_HASH -> $HEAD_HASH"
    echo ""

    # The head branch's legacy_delegates.toml must contain the base hash
    if git show "$HEAD_SHA:$TOML_PATH" | known_hashes_from_stdin | grep -qF "$BASE_HASH"; then
        echo "Old hash found in legacy_delegates.toml — migration entry exists."
        exit 0
    fi

    echo "FAILED: Old delegate hash $BASE_HASH is NOT in legacy_delegates.toml on HEAD!"
    echo ""
    echo "When the committed delegate WASM changes, the old hash MUST be added to"
    echo "legacy_delegates.toml so existing users can migrate their room data."
    echo ""
    echo "To fix:"
    echo "  1. Revert the WASM change, OR"
    echo "  2. Run: cargo make add-migration  (before committing the new WASM)"
    exit 1
else
    # Local mode: compare HEAD's committed WASM to the working-tree file
    [ -f "$WASM_PATH" ] || die "Committed WASM not found: $WASM_PATH"

    WORKING_HASH=$(wasm_hash_file "$WASM_PATH")
    HEAD_HASH=$(wasm_hash_at_ref "HEAD")

    if [ -z "$HEAD_HASH" ]; then
        echo "Committed WASM not tracked at HEAD — no migration needed (new file)."
        exit 0
    fi

    echo "Committed WASM hash at HEAD:    $HEAD_HASH"
    echo "Committed WASM hash in working: $WORKING_HASH"

    if [ "$HEAD_HASH" = "$WORKING_HASH" ]; then
        echo "Committed WASM matches HEAD — no migration needed."
        exit 0
    fi

    echo ""
    echo "Committed WASM was modified in working tree: $HEAD_HASH -> $WORKING_HASH"
    echo ""

    if grep '^code_hash' "$TOML_PATH" | grep -qF "$HEAD_HASH"; then
        echo "Old hash found in legacy_delegates.toml — migration entry exists."
        exit 0
    fi

    echo "Old hash NOT in legacy_delegates.toml!"
    echo ""
    echo "To fix:"
    echo "  1. Revert the WASM change: git checkout HEAD -- $WASM_PATH"
    echo "  2. OR run: cargo make add-migration  (before committing the new WASM)"
    exit 1
fi
