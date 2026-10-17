#!/usr/bin/env bash
#
# Refuse a webapp archive whose UI does not watch River's real pointer records.
#
# An open River tab reloads after a re-key only because the UI watches River's
# pointer records, signed by the author key in pointer-records.toml
# (freenet/river#580). `ui/build.rs` bakes that key into the UI WASM, unless
# RIVER_POINTER_RECORDS pointed the build at test records. A UI built that way
# watches pointers nobody publishes: published, its tabs would never reload,
# and nothing would say so. `dx build` hides the cargo warning, so the check is
# on the artifact itself: the author key string must be in the UI WASM.
#
# Runs from `sign-webapp`, after the archive is built and BEFORE the version
# counter is bumped, so a refusal burns no version.
#
# Usage: scripts/check-webapp-pointer-author.sh ARCHIVE [POINTER_RECORDS_TOML]
# Exit:  0 ok, 1 refused, 2 could not check.

set -euo pipefail

archive="${1:?usage: $0 ARCHIVE [POINTER_RECORDS_TOML]}"
toml="${2:-pointer-records.toml}"

author="$(sed -n 's/^author_verifying_key *= *"\([^"]*\)".*/\1/p' "$toml" | head -1)"
if [ -z "$author" ]; then
    echo "check-webapp-pointer-author: no author_verifying_key in $toml" >&2
    exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
if ! tar -xJf "$archive" -C "$work" 2>/dev/null; then
    echo "check-webapp-pointer-author: cannot unpack $archive" >&2
    exit 2
fi

# The UI's own WASM; contracts/ holds the room contract and delegate.
found=0
while IFS= read -r -d '' wasm; do
    found=1
    if grep -qaF "$author" "$wasm"; then
        echo "check-webapp-pointer-author: ok ($(basename "$wasm") watches $author)"
        exit 0
    fi
done < <(find "$work" -name '*.wasm' -not -path '*/contracts/*' -print0)

if [ "$found" -eq 0 ]; then
    echo "check-webapp-pointer-author: no UI WASM in $archive" >&2
    exit 2
fi

cat >&2 <<EOF
REFUSING to sign $archive: its UI does not carry River's pointer author key
($author from $toml).

It was almost certainly built with RIVER_POINTER_RECORDS set (a test build).
Published, its open tabs would never reload after a re-key. Unset
RIVER_POINTER_RECORDS, rebuild (cargo make build), and publish again.
EOF
exit 1
