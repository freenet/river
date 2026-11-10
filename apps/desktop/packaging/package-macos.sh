#!/bin/bash
# Assemble an UNSIGNED universal River.app from an arm64 and an x86_64 `river` binary.
#
#   package-macos.sh <arm64 binary> <x86_64 binary> <version> <output dir>
#
# Produces <output dir>/River.app. Signing, notarization and stapling are a separate
# step (sign-macos.sh), so this runs on pull requests with no secrets and a release
# signs exactly the bundle CI already checked. Runs on macOS only (lipo, plutil).

set -euo pipefail

ARM64_BIN="${1:?usage: package-macos.sh <arm64 binary> <x86_64 binary> <version> <output dir>}"
X86_BIN="${2:?x86_64 binary required}"
VERSION="${3:?version required}"
OUT_DIR="${4:?output dir required}"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "package-macos.sh: needs macOS (found $(uname -s))" >&2
    exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
APP="$OUT_DIR/River.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

echo ">> Universal binary"
lipo -create -output "$APP/Contents/MacOS/river" "$ARM64_BIN" "$X86_BIN"
chmod 0755 "$APP/Contents/MacOS/river"
archs="$(lipo -archs "$APP/Contents/MacOS/river")"
echo "architectures: $archs"
for want in arm64 x86_64; do
    if [[ " $archs " != *" $want "* ]]; then
        echo "package-macos.sh: universal binary lacks $want (has: $archs)" >&2
        exit 1
    fi
done

# This runs the slice native to the build machine (arm64 on CI). The x86_64 slice is
# compiled and linked but never executed in CI; only an Intel Mac runs it.
reported="$("$APP/Contents/MacOS/river" --version)"
if [[ "$reported" != "river $VERSION" ]]; then
    echo "package-macos.sh: binary reports '$reported', expected 'river $VERSION'" >&2
    exit 1
fi

echo ">> Info.plist"
sed "s/@VERSION@/$VERSION/g" "$HERE/macos/Info.plist" > "$APP/Contents/Info.plist"
if grep -q '@[A-Z_]*@' "$APP/Contents/Info.plist"; then
    echo "package-macos.sh: unreplaced placeholder in Info.plist" >&2
    exit 1
fi
plutil -lint "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp "$HERE/icons/River.icns" "$APP/Contents/Resources/River.icns"

echo ">> $APP"
find "$APP" -print
