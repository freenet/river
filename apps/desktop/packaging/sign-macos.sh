#!/bin/bash
# Sign, notarize and staple River.app, then zip it for release.
#
#   sign-macos.sh <River.app> <output .zip>
#
# Environment (all required; the script fails if any is empty):
#   CODESIGN_IDENTITY   "Developer ID Application: Ian CLARKE (R55ZESJCXG)"
#   EXPECTED_TEAM_ID    R55ZESJCXG, checked against the signature
#   ASC_API_KEY_PATH    App Store Connect API key (.p8) file, for notarytool
#   ASC_API_KEY_ID      its key ID
#   ASC_API_ISSUER_ID   its issuer UUID
#
# The signing identity must already be in a keychain codesign can search.
#
# Hardened runtime, with NO entitlements. Each entitlement switches off one hardened
# runtime protection, and River needs none of them: it runs no JIT and no WASM (unlike
# the Freenet node, which needs allow-jit for wasmtime), loads no unsigned libraries
# (its GPU path is Apple's Metal), and reads no protected data. Network access, starting
# the Freenet node, `hdiutil`/`ditto` during install and `open` for the browser are all
# allowed without one. The Freenet.app it installs carries its own signature and
# entitlements; a child process does not inherit River's.

set -euo pipefail

APP="${1:?usage: sign-macos.sh <River.app> <output .zip>}"
OUT_ZIP="${2:?output .zip required}"

for v in CODESIGN_IDENTITY EXPECTED_TEAM_ID ASC_API_KEY_PATH ASC_API_KEY_ID ASC_API_ISSUER_ID; do
    if [[ -z "${!v:-}" ]]; then
        echo "sign-macos.sh: $v is not set" >&2
        exit 1
    fi
done
[[ -d "$APP" ]] || { echo "sign-macos.sh: $APP is not a directory" >&2; exit 1; }
[[ -f "$ASC_API_KEY_PATH" ]] || { echo "sign-macos.sh: $ASC_API_KEY_PATH not found" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Fails unless the bundle is validly signed by our team, with the hardened runtime.
check_signature() {
    local app="$1" info
    codesign --verify --deep --strict --verbose=2 "$app"
    info="$(codesign --display --verbose=4 "$app" 2>&1)"
    echo "$info"
    if ! grep -qxF "TeamIdentifier=$EXPECTED_TEAM_ID" <<<"$info"; then
        echo "sign-macos.sh: $app is not signed by team $EXPECTED_TEAM_ID" >&2
        exit 1
    fi
    if ! grep -qxF "Authority=$CODESIGN_IDENTITY" <<<"$info"; then
        echo "sign-macos.sh: $app is not signed by '$CODESIGN_IDENTITY'" >&2
        exit 1
    fi
    if ! grep -q 'flags=.*(runtime)' <<<"$info"; then
        echo "sign-macos.sh: $app lacks the hardened runtime" >&2
        exit 1
    fi
}

echo ">> Signing $APP"
# One executable and no nested code, so signing the bundle signs everything. --timestamp
# is required for Developer ID; --options runtime is required for notarization.
codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$APP"
check_signature "$APP"

echo ">> Notarizing"
ditto -c -k --keepParent "$APP" "$WORK/notarize.zip"
xcrun notarytool submit "$WORK/notarize.zip" \
    --key "$ASC_API_KEY_PATH" --key-id "$ASC_API_KEY_ID" --issuer "$ASC_API_ISSUER_ID" \
    --wait --output-format json > "$WORK/notary.json"
cat "$WORK/notary.json"
echo
status="$(plutil -extract status raw -o - "$WORK/notary.json")"
id="$(plutil -extract id raw -o - "$WORK/notary.json")"
if [[ "$status" != "Accepted" ]]; then
    echo "sign-macos.sh: notarization status is '$status'; the log says why:" >&2
    xcrun notarytool log "$id" \
        --key "$ASC_API_KEY_PATH" --key-id "$ASC_API_KEY_ID" --issuer "$ASC_API_ISSUER_ID" >&2 || true
    exit 1
fi

echo ">> Stapling"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

echo ">> Gatekeeper assessment"
assessment="$(spctl --assess --type exec --verbose=4 "$APP" 2>&1)"
echo "$assessment"
if ! grep -q 'source=Notarized Developer ID' <<<"$assessment"; then
    echo "sign-macos.sh: Gatekeeper does not accept $APP as a notarized Developer ID app" >&2
    exit 1
fi

echo ">> $OUT_ZIP"
mkdir -p "$(dirname "$OUT_ZIP")"
rm -f "$OUT_ZIP"
ditto -c -k --keepParent "$APP" "$OUT_ZIP"

# Check what users will download, not only the bundle it was made from.
ditto -x -k "$OUT_ZIP" "$WORK/unzipped"
check_signature "$WORK/unzipped/River.app"
xcrun stapler validate "$WORK/unzipped/River.app"
