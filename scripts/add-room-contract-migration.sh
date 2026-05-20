#!/bin/bash
# Add a migration entry for the currently committed room-contract WASM.
#
# Run this BEFORE your change rebuilds room_contract.wasm: it records the OLD
# contract generation's BLAKE3 code hash in common/legacy_room_contracts.toml,
# so clients can probe the old contract key and recover a room that was dormant
# across the upgrade (freenet/river#292). If your changes already rebuilt the
# WASM, restore it first: `git checkout HEAD -- ui/public/contracts/`.
#
# Usage:
#   scripts/add-room-contract-migration.sh "V25" "Description of what changed"
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML="$REPO_ROOT/common/legacy_room_contracts.toml"
COMMITTED="$REPO_ROOT/ui/public/contracts/room_contract.wasm"

die() { echo "ERROR: $*" >&2; exit 1; }

command -v b3sum >/dev/null 2>&1 || die "b3sum not found. Install with: cargo install b3sum"
[ -f "$COMMITTED" ] || die "Committed room-contract WASM not found: $COMMITTED"
[ -f "$TOML" ] || die "Registry not found: $TOML"

VERSION="${1:?Usage: add-room-contract-migration.sh VERSION DESCRIPTION}"
DESCRIPTION="${2:?Usage: add-room-contract-migration.sh VERSION DESCRIPTION}"

# code_hash = BLAKE3(wasm) — exactly what CodeHash::from_code computes.
CODE_HASH=$(b3sum "$COMMITTED" | cut -d' ' -f1)

echo "Committed room-contract WASM: $COMMITTED"
echo "  code_hash: $CODE_HASH"

if grep -qF "$CODE_HASH" "$TOML"; then
    echo ""
    echo "This code_hash is already in $TOML — no action needed."
    exit 0
fi

DATE=$(date +%Y-%m-%d)
cat >> "$TOML" << EOF

[[entry]]
version = "$VERSION"
description = "$DESCRIPTION"
date = "$DATE"
code_hash = "$CODE_HASH"
EOF

echo ""
echo "Added $VERSION to $TOML"
echo ""
echo "Next steps:"
echo "  1. cargo make sync-wasm     # rebuild + copy the NEW room-contract WASM"
echo "  2. cargo test -p river-core --test room_contract_migration_test"
echo "  3. git add common/legacy_room_contracts.toml ui/public/contracts/ cli/contracts/"
echo "  4. git commit"
