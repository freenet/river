#!/usr/bin/env bash
#
# Tests for scripts/previous-tag.sh, which picks --notes-start-tag for the
# riverctl and desktop release workflows. Those run only on tag pushes, so this
# is the only place the selection runs before a release (freenet/river#614).
#
# Run: ./scripts/tests/previous-tag-test.sh

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo_root/scripts/previous-tag.sh"

failures=0
checks=0

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
repo="$work/repo"
git init -q -b main "$repo"
git -C "$repo" -c user.email=t@example.invalid -c user.name=t -c commit.gpgsign=false \
    commit -q --allow-empty -m init
for t in v0.1.51 riverctl-v0.2.9 riverctl-v0.2.10 riverctl-v0.2.25 \
         river-desktop-v0.1.0 river-desktop-v0.2.0-rc.1 river-desktop-v0.2.0-rc.2 \
         river-desktop-v0.2.0 river-desktop-v0.10.0; do
    git -C "$repo" tag "$t"
done

expect() { # <prefix> <tag> <expected previous, or empty>
    local got rc
    got="$(cd "$repo" && "$script" "$1" "$2")"
    rc=$?
    checks=$((checks + 1))
    # The exit status is checked too: an empty expectation must not pass because
    # the script crashed and printed nothing.
    if [[ $rc -ne 0 ]]; then
        failures=$((failures + 1))
        printf 'FAIL %s -> script exited %d\n' "$2" "$rc"
    elif [[ "$got" == "$3" ]]; then
        printf 'ok   %s -> %s\n' "$2" "${3:-<none>}"
    else
        failures=$((failures + 1))
        printf 'FAIL %s -> expected %s, got %s\n' "$2" "${3:-<none>}" "${got:-<none>}"
    fi
}

expect riverctl-v riverctl-v0.2.25 riverctl-v0.2.10       # numeric, not lexical
expect riverctl-v riverctl-v0.2.10 riverctl-v0.2.9
expect riverctl-v riverctl-v0.2.9 ""                      # first of its family
expect river-desktop-v river-desktop-v0.1.0 ""            # other families ignored
expect river-desktop-v river-desktop-v0.2.0-rc.1 river-desktop-v0.1.0
expect river-desktop-v river-desktop-v0.2.0-rc.2 river-desktop-v0.2.0-rc.1
expect river-desktop-v river-desktop-v0.2.0 river-desktop-v0.2.0-rc.2   # rc before release
expect river-desktop-v river-desktop-v0.10.0 river-desktop-v0.2.0
expect river-desktop-v river-desktop-v9.9.9 ""             # tag not in the list

printf '\n%d checks, %d failed\n' "$checks" "$failures"
[[ $failures -eq 0 ]]
