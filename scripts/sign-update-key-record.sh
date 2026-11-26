#!/bin/bash
# Sign River's update-key record (apps/desktop/update-authority.toml) and print what to publish.
#
# Offline: it needs the authority (author) key, not the network. Publishing is separate, see
# the header of apps/desktop/update-authority.toml for the exact fdev commands.
#
# Usage:
#   scripts/sign-update-key-record.sh --version N --key <64 hex digits of the NEW update-signing public key>
#   scripts/sign-update-key-record.sh --version N --withdraw
#
# The key file is $RIVER_POINTER_KEY_FILE, else ~/.config/river/web-container-keys.toml (the
# same author key scripts/sign-pointer-records.sh uses). It is piped straight into the signer
# and never lands in a variable, temp file or argv.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="apps/desktop/update-authority.toml"
KEY_FILE="${RIVER_POINTER_KEY_FILE:-$HOME/.config/river/web-container-keys.toml}"
cd "$REPO_ROOT"

die() { echo "ERROR: $*" >&2; exit 1; }

VERSION=""; NEW_KEY=""; WITHDRAW=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --key) NEW_KEY="${2:?--key needs a value}"; shift 2 ;;
        --withdraw) WITHDRAW=1; shift ;;
        *) die "unknown argument '$1'" ;;
    esac
done
[[ "$VERSION" =~ ^[1-9][0-9]*$ ]] || die "--version N (a positive integer, higher than every earlier record) is required"
if [ "$WITHDRAW" = 1 ]; then
    [ -z "$NEW_KEY" ] || die "--withdraw and --key are exclusive"
    NEW_KEY="0000000000000000000000000000000000000000000000000000000000000000"
else
    [[ "$NEW_KEY" =~ ^[0-9a-fA-F]{64}$ ]] || die "--key must be the new update-signing PUBLIC key, 64 hex digits (see apps/desktop/update-pubkey.hex)"
    [[ ! "$NEW_KEY" =~ ^0+$ ]] || die "an all-zero key is the withdrawal: use --withdraw"
fi

command -v pointer-record >/dev/null 2>&1 || die "pointer-record not found (see scripts/check-pointer-freshness.sh for the install line)"
[ -f "$KEY_FILE" ] || die "key file not found: $KEY_FILE (set RIVER_POINTER_KEY_FILE)"
case "$(stat -c '%a' "$KEY_FILE")" in
    600|400) ;;
    *) die "$KEY_FILE must be mode 600" ;;
esac
[ -f "$TOML_PATH" ] || die "$TOML_PATH not found"

field() { sed -n "s/^$1 *= *\"\(.*\)\"/\1/p" "$TOML_PATH" | head -n1; }
AUTHOR_VK="$(field authority_verifying_key)"
APP_ID="$(field app_id)"
[ -n "$AUTHOR_VK" ] && [ -n "$APP_ID" ] || die "could not read authority_verifying_key and app_id from $TOML_PATH"

# --author-vk makes the signer REFUSE a key file that is not the authority named in the toml.
OUT="$(pointer-record sign --author-vk "$AUTHOR_VK" --app-id "$APP_ID" \
    --version "$VERSION" --code-hash "$NEW_KEY" < "$KEY_FILE")" || die "signing failed"
STATE="$(printf '%s\n' "$OUT" | sed -n 's/^state=//p')"
ADDR="$(printf '%s\n' "$OUT" | sed -n 's/^key=//p')"
[ -n "$STATE" ] && [ -n "$ADDR" ] || die "the signer produced no state/key"
pointer-record verify --author-vk "$AUTHOR_VK" --app-id "$APP_ID" --state "$STATE" \
    --expect-version "$VERSION" --expect-code-hash "$NEW_KEY" >/dev/null || die "the signed record does not verify"
TOML_KEY="$(field pointer_key)"
[ "$ADDR" = "$TOML_KEY" ] || die "signer address $ADDR != pointer_key $TOML_KEY in $TOML_PATH"

echo "record version $VERSION naming ${NEW_KEY}"
echo "address: $ADDR"
echo "state:   $STATE"
echo
echo "Next: publish it (header of $TOML_PATH), and in the next release set"
echo "  pinned_record = \"$STATE\""
echo "in $TOML_PATH so fresh installs cannot adopt an older record."
