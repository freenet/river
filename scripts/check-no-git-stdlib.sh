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
# Every workspace lock in the repo (root, apps/, tools/...), not only the root one: apps/ is a
# separate workspace with its own Cargo.lock and Cargo.toml.
while IFS= read -r lock; do
    dir="$(dirname "$lock")"
    rel="${lock#"$root"/}"
    if awk '/^name = "freenet-(stdlib|macros)"$/ {f=1; next} /^$/ {f=0} f && /^source = "git\+/ {found=1} END {exit !found}' "$lock"; then
        echo "ERROR: $rel resolves freenet-stdlib or freenet-macros from a git source." >&2
        bad=1
    fi
    if [ -f "$dir/Cargo.toml" ] && grep -Eq '^freenet-(stdlib|macros)[[:space:]]*=.*git[[:space:]]*=' "$dir/Cargo.toml"; then
        echo "ERROR: ${rel%Cargo.lock}Cargo.toml patches or depends on freenet-stdlib or freenet-macros from git." >&2
        bad=1
    fi
done < <(find "$root" -name Cargo.lock -not -path '*/target/*' -not -path '*/node_modules/*' | sort)
[ -f "$root/Cargo.lock" ] || { echo "ERROR: no Cargo.lock at $root" >&2; exit 1; }
if [ "$bad" -ne 0 ]; then
    echo "Publish and release builds need the released stdlib: drop the patch once it ships." >&2
    exit 1
fi
echo "OK: freenet-stdlib and freenet-macros resolve from the registry."
