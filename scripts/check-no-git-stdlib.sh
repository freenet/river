#!/bin/bash
# Fail when freenet-stdlib or freenet-macros resolves from a git source in
# Cargo.lock, or when Cargo.toml patches either from git. A WASM built that way
# embeds the git checkout path and short rev, so its hash differs from one
# built from the released crate: publish and release builds must not use it.
# Other [patch] entries (e.g. markdown) are fine.
# Usage: scripts/check-no-git-stdlib.sh [repo-root]   (default: the repo)
set -euo pipefail
root="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
bad=0
if awk '/^name = "freenet-(stdlib|macros)"$/ {f=1; next} /^$/ {f=0} f && /^source = "git\+/ {found=1} END {exit !found}' "$root/Cargo.lock"; then
    echo "ERROR: Cargo.lock resolves freenet-stdlib or freenet-macros from a git source." >&2
    bad=1
fi
if grep -Eq '^freenet-(stdlib|macros)[[:space:]]*=.*git[[:space:]]*=' "$root/Cargo.toml"; then
    echo "ERROR: Cargo.toml patches or depends on freenet-stdlib or freenet-macros from git." >&2
    bad=1
fi
if [ "$bad" -ne 0 ]; then
    echo "Publish and release builds need the released stdlib: drop the patch once it ships." >&2
    exit 1
fi
echo "OK: freenet-stdlib and freenet-macros resolve from the registry."
