#!/bin/bash
# Regenerate the desktop app's icons from the River logo.
#
#   apps/desktop/packaging/generate-icons.sh
#
# Writes, under apps/desktop/packaging/icons/:
#   river.png   512x512, shipped in the Linux tarball next to river.desktop
#   river.ico   16-256 px, compiled into river.exe by apps/desktop/build.rs
#   River.icns  16-1024 px, copied into River.app/Contents/Resources
#
# The outputs are committed rather than generated during the build. The Windows
# build needs river.ico before cargo runs (build.rs embeds it), and a stock
# windows-latest runner has no SVG renderer, so generating it there would add a
# package download to every release build. Committing all three keeps the
# release builds free of extra tool installs and lets a reviewer see the icons
# in the PR. Re-run this script when ui/assets/river_logo.svg changes.
#
# Requires rsvg-convert (librsvg2-bin / brew librsvg), ImageMagick `convert`
# for the .ico, and iconutil (macOS) or png2icns (libicns, Linux) for the .icns.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$HERE/../../.." && pwd)"
SVG="$REPO_ROOT/ui/assets/river_logo.svg"
OUT="$HERE/icons"

for tool in rsvg-convert convert; do
    command -v "$tool" >/dev/null 2>&1 || { echo "generate-icons.sh: $tool is required" >&2; exit 1; }
done
[[ -f "$SVG" ]] || { echo "generate-icons.sh: $SVG not found" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$OUT"

render() { rsvg-convert -w "$1" -h "$1" "$SVG" -o "$2"; }

# Linux
render 512 "$OUT/river.png"

# Windows: one image per standard size, so Explorer never scales a large one down.
ico_pngs=()
for size in 16 24 32 48 64 128 256; do
    render "$size" "$WORK/ico_$size.png"
    ico_pngs+=("$WORK/ico_$size.png")
done
convert "${ico_pngs[@]}" "$OUT/river.ico"

# macOS
if command -v iconutil >/dev/null 2>&1; then
    ICONSET="$WORK/River.iconset"
    mkdir -p "$ICONSET"
    for size in 16 32 128 256 512; do
        render "$size" "$ICONSET/icon_${size}x${size}.png"
        render $((size * 2)) "$ICONSET/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$ICONSET" -o "$OUT/River.icns"
elif command -v png2icns >/dev/null 2>&1; then
    # libicns picks the slot from the pixel size, so give it one PNG per size.
    icns_pngs=()
    for size in 16 32 128 256 512 1024; do
        render "$size" "$WORK/icns_$size.png"
        icns_pngs+=("$WORK/icns_$size.png")
    done
    png2icns "$OUT/River.icns" "${icns_pngs[@]}" >/dev/null
else
    echo "generate-icons.sh: need iconutil (macOS) or png2icns (libicns)" >&2
    exit 1
fi

ls -l "$OUT"
