#!/bin/bash
# Test scripts/check-no-git-stdlib.sh against fixture Cargo.lock/Cargo.toml.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
guard="$here/../check-no-git-stdlib.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

mk() { # <name> <stdlib source line> <toml patch line>
    mkdir -p "$tmp/$1"
    printf '[[package]]\nname = "markdown"\nversion = "1.0.0"\nsource = "git+https://github.com/freenet/markdown-rs?rev=abc#abc"\n\n[[package]]\nname = "freenet-macros"\nversion = "0.3.1"\nsource = "%s"\n\n[[package]]\nname = "freenet-stdlib"\nversion = "0.12.1"\nsource = "%s"\n' "$2" "$2" > "$tmp/$1/Cargo.lock"
    printf '[patch.crates-io]\nmarkdown = { git = "https://github.com/freenet/markdown-rs", rev = "abc" }\n%s\n' "$3" > "$tmp/$1/Cargo.toml"
}
reg="registry+https://github.com/rust-lang/crates.io-index"
git="git+https://github.com/freenet/freenet-stdlib?rev=6bd4398c#6bd4398c"

mk ok "$reg" ""
mk lock-git "$git" ""
mk toml-git "$reg" 'freenet-stdlib = { git = "https://github.com/freenet/freenet-stdlib", rev = "6bd4398c" }'

"$guard" "$tmp/ok" >/dev/null || { echo "FAIL: registry stdlib with a markdown git patch must pass"; exit 1; }
for c in lock-git toml-git; do
    if "$guard" "$tmp/$c" >/dev/null 2>&1; then echo "FAIL: $c must be refused"; exit 1; fi
done
echo "PASS: check-no-git-stdlib"
