#!/bin/bash
# Sign River's update-key record (apps/desktop/update-authority.toml) and print what to publish.
#
# Offline: it needs the authority key, not the network. Publishing is separate, see
# the header of apps/desktop/update-authority.toml for the exact fdev commands.
#
# Usage:
#   scripts/sign-update-key-record.sh --version N --key <64 hex digits of the NEW update-signing public key>
#   scripts/sign-update-key-record.sh --version N --withdraw
#
# The authority key is held offline. Give it as a file in $RIVER_UPDATE_AUTHORITY_KEY_FILE, or on
# stdin (for example from the password manager): an armored block whose lines include
# `signing_key = "river:v1:sk:..."`. There is no default location. It is piped straight into
# the signer and never lands in a variable, temp file or argv, and is never printed.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOML_PATH="apps/desktop/update-authority.toml"
KEY_FILE="${RIVER_UPDATE_AUTHORITY_KEY_FILE:-}"
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
    # A canonical point that is not weak: what the app itself requires of a key in a record.
    # RIVER_UPDATE_TOOL may name a prebuilt update_tool; otherwise it is built from apps/.
    if [ -n "${RIVER_UPDATE_TOOL:-}" ]; then
        "$RIVER_UPDATE_TOOL" check-key "$NEW_KEY" || die "--key is not an acceptable update-signing public key"
    else
        cargo run -q --manifest-path apps/Cargo.toml -p river-desktop --example update_tool -- check-key "$NEW_KEY" \
            || die "--key is not an acceptable update-signing public key (or update_tool did not build)"
    fi
fi

command -v pointer-record >/dev/null 2>&1 || die "pointer-record not found (see scripts/check-pointer-freshness.sh for the install line)"
if [ -n "$KEY_FILE" ]; then
    [ -f "$KEY_FILE" ] || die "key file not found: $KEY_FILE"
    # GNU stat, else BSD/macOS stat.
    MODE="$(stat -c '%a' "$KEY_FILE" 2>/dev/null || stat -f '%Lp' "$KEY_FILE")"
    case "$MODE" in
        600|400) ;;
        *) die "$KEY_FILE must be mode 600" ;;
    esac
    exec 3< "$KEY_FILE"
else
    [ ! -t 0 ] || die "give the authority key on stdin or set RIVER_UPDATE_AUTHORITY_KEY_FILE (no default: it is held offline)"
    exec 3<&0
fi
[ -f "$TOML_PATH" ] || die "$TOML_PATH not found"

field() { sed -n "s/^$1 *= *\"\(.*\)\"/\1/p" "$TOML_PATH" | head -n1; }
PINNED="$(field pinned_record)"
if [ -n "$PINNED" ]; then
    # The first four bytes of a record are its big-endian version.
    PINNED_VERSION=$((16#${PINNED:0:8}))
    [ "$VERSION" -gt "$PINNED_VERSION" ] || die "--version $VERSION is not above the pinned record's version $PINNED_VERSION: builds ignore it"
fi
AUTHOR_VK="$(field authority_verifying_key)"
APP_ID="$(field app_id)"
[ -n "$AUTHOR_VK" ] && [ -n "$APP_ID" ] || die "could not read authority_verifying_key and app_id from $TOML_PATH"

# --author-vk makes the signer REFUSE a key that is not the authority named in the toml.
OUT="$(pointer-record sign --author-vk "$AUTHOR_VK" --app-id "$APP_ID" \
    --version "$VERSION" --code-hash "$NEW_KEY" <&3)" || die "signing failed"
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
