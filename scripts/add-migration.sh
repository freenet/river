#!/bin/bash
# Add a migration entry for the currently committed delegate WASM.
#
# Usage:
#   scripts/add-migration.sh "V7" "Description of what changed"
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/legacy_delegates.toml"
COMMITTED="$REPO_ROOT/ui/public/contracts/chat_delegate.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$COMMITTED" ] || die "Committed WASM not found: $COMMITTED"

VERSION="${1:?Usage: add-migration.sh VERSION DESCRIPTION}"
DESCRIPTION="${2:?Usage: add-migration.sh VERSION DESCRIPTION}"

# Compute code_hash = BLAKE3(wasm)
CODE_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)

# Compute delegate_key = BLAKE3(code_hash_bytes)
# Convert hex to raw bytes and hash again
DELEGATE_KEY=$(echo -n "$CODE_HASH" | xxd -r -p | b3sum --no-names)

echo "Committed delegate WASM: $COMMITTED"
echo "  code_hash:    $CODE_HASH"
echo "  delegate_key: $DELEGATE_KEY"

# Check if already exists
if grep -qF "$CODE_HASH" "$TOML"; then
    echo ""
    echo "This code_hash is already in $TOML — no action needed."
    exit 0
fi

# Append entry
DATE=$(date +%Y-%m-%d)
cat >> "$TOML" << EOF

[[entry]]
version = "$VERSION"
description = "$DESCRIPTION"
date = "$DATE"
delegate_key = "$DELEGATE_KEY"
code_hash = "$CODE_HASH"
EOF

echo ""
echo "Added $VERSION to $TOML"
echo ""
echo "Next steps:"
echo "  1. cargo make sync-wasm     # copy new WASMs to committed locations"
echo "  2. cargo check -p river-ui  # verify build with new migration entry"
echo "  3. git add legacy_delegates.toml ui/public/contracts/ cli/contracts/"
echo "  4. git commit"
