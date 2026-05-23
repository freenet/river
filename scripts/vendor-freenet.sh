#!/bin/bash
# Recreate vendor/freenet/ from the published freenet 0.2.61 crate,
# stripping the Windows/macOS GUI dep blocks that conflict with
# dioxus-desktop's wry/tao versions at Cargo resolution time.
#
# Run this once after cloning if you intend to build the Android target.
# The vendored copy is NOT committed (see .gitignore) — this script is the
# reproducible source of truth.
#
# See `[patch.crates-io] freenet` in the workspace `Cargo.toml`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="0.2.61"
DEST="$REPO_ROOT/vendor/freenet"

if [ -d "$DEST" ]; then
    echo "vendor/freenet/ already exists; rm -rf it first to refresh."
    exit 0
fi

echo "Downloading freenet $VERSION crate from crates.io …"
mkdir -p "$REPO_ROOT/vendor"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -sL "https://crates.io/api/v1/crates/freenet/$VERSION/download" \
    | tar -xz -C "$tmp"
mv "$tmp/freenet-$VERSION" "$DEST"

echo "Stripping Windows/macOS GUI dep blocks (binary-only, never compiled \
for Android, conflicts with dioxus-desktop's wry 0.53 / tao 0.34) …"
python3 - "$DEST/Cargo.toml" <<'PY'
import re, sys
p = sys.argv[1]
src = open(p).read()
removed = []
def kill(pat, label):
    global src
    pattern = r'^' + pat + r'\n(?:(?!^\[).*\n)*'
    new, n = re.subn(pattern, '', src, flags=re.MULTILINE)
    if n:
        removed.append(label); src = new
for name in ['muda', 'tao', 'tray-icon']:
    kill(rf'\[target\.\'cfg\(target_os = "macos"\)\'\.dependencies\.{re.escape(name)}\]', f'macos {name}')
for name in ['muda', 'serde', 'tao', 'tray-icon', 'winapi', 'winreg', 'wry', 'zip']:
    kill(rf'\[target\."cfg\(windows\)"\.dependencies\.{re.escape(name)}\]', f'win {name}')
kill(r'\[target\."cfg\(windows\)"\.build-dependencies\.winres\]', 'win build winres')
open(p, 'w').write(src)
print("removed:", removed)
PY

echo ""
echo "✓ vendor/freenet/ ready. The workspace [patch.crates-io] entry will"
echo "  redirect the published freenet crate to this local copy."
