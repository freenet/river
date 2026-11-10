#!/bin/bash
# Refuse to release the desktop app from a commit that should not ship.
#
#   verify-release.sh <tag> <commit>
#
# Checks, against the git history (origin/main must be fetched; MAIN_REF overrides it):
#
#   1. The tag is river-desktop-v<version>, and <version> is the [workspace.package]
#      version in apps/Cargo.toml at <commit>.
#   2. <commit> is on main (publish from main only).
#   3. ui/public/contracts/ at <commit> is byte-identical to ui/public/contracts/ at the
#      latest "chore(publish): bump webapp contract version" commit on main. The app
#      compiles in chat_delegate.wasm (the delegate key it reads the user's rooms
#      under) and room_contract.wasm (part of every room's contract key), and it cannot
#      migrate from one key to another. So it must carry exactly the WASM the live River
#      web UI carries. The check is:
#
#        bump=$(git log origin/main -1 --format=%H \
#                 --grep='^chore(publish): bump webapp contract version')
#        git diff --quiet "$bump" <commit> -- ui/public/contracts/
#
#      That commit is merged after each River UI publish (river-publish.md, step 6), so
#      its tree is what was published. If the WASM changed since, publish the River UI
#      first and tag a commit at or after its bump commit.
#
# On success prints `version=<version>` and `webapp_version=<published webapp version>`,
# ready to append to $GITHUB_OUTPUT.

set -euo pipefail

TAG="${1:?usage: verify-release.sh <tag> <commit>}"
COMMIT="${2:?commit required}"
MAIN_REF="${MAIN_REF:-origin/main}"
CONTRACTS=ui/public/contracts/
BUMP_SUBJECT='^chore(publish): bump webapp contract version'

fail() { echo "::error::$*" >&2; exit 1; }

COMMIT="$(git rev-parse --verify "$COMMIT^{commit}")"

tag_version="${TAG#river-desktop-v}"
[[ "$tag_version" != "$TAG" && -n "$tag_version" ]] || fail "Tag '$TAG' is not of the form river-desktop-v<version>."

cargo_version="$(git show "$COMMIT:apps/Cargo.toml" | python3 -c \
    'import sys, tomllib; print(tomllib.loads(sys.stdin.read())["workspace"]["package"]["version"])')"
echo "tag version:           $tag_version" >&2
echo "apps/Cargo.toml:       $cargo_version" >&2
[[ "$tag_version" == "$cargo_version" ]] \
    || fail "Tag version ($tag_version) does not match apps/Cargo.toml ($cargo_version). Bump [workspace.package] version, merge, then tag that commit."

git merge-base --is-ancestor "$COMMIT" "$MAIN_REF" \
    || fail "Commit $COMMIT is not on $MAIN_REF. The desktop app is released only from main."

bump="$(git log "$MAIN_REF" -1 --format=%H --grep="$BUMP_SUBJECT")"
[[ -n "$bump" ]] || fail "No '$BUMP_SUBJECT' commit found on $MAIN_REF."
echo "latest publish bump:   $(git log -1 --format='%h %s' "$bump")" >&2
if ! git diff --quiet "$bump" "$COMMIT" -- "$CONTRACTS"; then
    git diff --stat "$bump" "$COMMIT" -- "$CONTRACTS" >&2
    fail "$CONTRACTS at $COMMIT differs from the last published River UI ($bump). The app would carry a delegate or contract the live web UI does not. Publish the River UI first, then tag a commit at or after its bump."
fi

webapp_version="$(git show "$bump:published-contract/contract-version.txt" | tr -d '[:space:]')"
[[ -n "$webapp_version" ]] || fail "published-contract/contract-version.txt is empty at $bump."
echo "published web UI:      $webapp_version" >&2

echo "version=$tag_version"
echo "webapp_version=$webapp_version"
