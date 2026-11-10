#!/bin/bash
# Refuse to release the desktop app from a commit that should not ship.
#
#   verify-release.sh <tag> <commit>
#
# Checks, against the git history (origin/main must be fetched; MAIN_REF overrides it):
#
#   1. The tag is river-desktop-v<version> with a semver <version>, and <version> is the
#      [workspace.package] version in apps/Cargo.toml at <commit>.
#   2. <commit> is on main (publish from main only). DRY_RUN=1 skips this check only,
#      for the workflow_dispatch rehearsal of a branch; a dry run never signs or releases.
#   3. ui/public/contracts/ at <commit> is byte-identical to ui/public/contracts/ at the
#      latest "chore(publish): bump webapp contract version" commit on main's first-parent
#      history. The app compiles in chat_delegate.wasm (the delegate key it reads the
#      user's rooms under) and room_contract.wasm (part of every room's contract key), and
#      it cannot migrate from one key to another. So it must carry exactly the WASM the
#      live River web UI carries. The check is:
#
#        bump=$(git log origin/main --first-parent -1 --format=%H \
#                 --grep='^chore(publish): bump webapp contract version')
#        git diff --quiet "$bump" <commit> -- ui/public/contracts/
#
#      ASSUMPTION: the bump PR is merged right after its publish (river-publish.md, step
#      6) and no WASM change merges to main in between, so the bump commit's tree holds
#      the published WASM. Nothing enforces that; check 4 narrows it.
#   4. The BLAKE3 code hash of each of those two WASM files at <commit> equals the
#      code_hash its record in pointer-records.toml carries at <commit>. Those records
#      are re-signed with every WASM change that gets published (river-publish.md, step
#      3b), so a mismatch means the tree and the published records disagree. Fails
#      closed if a record is missing. B3SUM overrides the hasher (default `b3sum`).
#
# On success prints, ready to append to $GITHUB_OUTPUT:
#   version, webapp_version, delegate_hash, contract_hash

set -euo pipefail

TAG="${1:?usage: verify-release.sh <tag> <commit>}"
COMMIT="${2:?commit required}"
MAIN_REF="${MAIN_REF:-origin/main}"
B3SUM="${B3SUM:-b3sum}"
CONTRACTS=ui/public/contracts/
DELEGATE_WASM=ui/public/contracts/chat_delegate.wasm
CONTRACT_WASM=ui/public/contracts/room_contract.wasm
BUMP_SUBJECT='^chore(publish): bump webapp contract version'

fail() { echo "::error::$*" >&2; exit 1; }

COMMIT="$(git rev-parse --verify "$COMMIT^{commit}")" || fail "Commit '$2' not found."

# 1. Tag and version.
[[ "$TAG" == river-desktop-v* ]] || fail "Tag '$TAG' is not of the form river-desktop-v<version>."
tag_version="${TAG#river-desktop-v}"
semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$'
[[ "$tag_version" =~ $semver ]] || fail "Tag '$TAG' does not end in a semver version."

# Plain awk rather than a TOML library: this also runs in CI on runners whose python3
# predates tomllib (scripts/tests/verify-release-test.sh).
cargo_version="$(git show "$COMMIT:apps/Cargo.toml" | awk '
    /^[[:space:]]*\[/ { in_pkg = ($0 ~ /^[[:space:]]*\[workspace\.package\][[:space:]]*$/); next }
    in_pkg && /^[[:space:]]*version[[:space:]]*=/ {
        sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
    }')"
[[ -n "$cargo_version" ]] || fail "No [workspace.package] version in apps/Cargo.toml at $COMMIT."
echo "tag version:           $tag_version" >&2
echo "apps/Cargo.toml:       $cargo_version" >&2
[[ "$tag_version" == "$cargo_version" ]] \
    || fail "Tag version ($tag_version) does not match apps/Cargo.toml ($cargo_version). Bump [workspace.package] version, merge, then tag that commit."

# 2. On main.
if [[ "${DRY_RUN:-0}" == "1" ]]; then
    echo "DRY RUN: not checking that $COMMIT is on $MAIN_REF." >&2
else
    git merge-base --is-ancestor "$COMMIT" "$MAIN_REF" \
        || fail "Commit $COMMIT is not on $MAIN_REF. The desktop app is released only from main."
fi

# 3. Same WASM as the last published web UI.
bump="$(git log "$MAIN_REF" --first-parent -1 --format=%H --grep="$BUMP_SUBJECT")"
[[ -n "$bump" ]] || fail "No '$BUMP_SUBJECT' commit found on $MAIN_REF."
echo "latest publish bump:   $(git log -1 --format='%h %s' "$bump")" >&2
if ! git diff --quiet "$bump" "$COMMIT" -- "$CONTRACTS"; then
    git diff --stat "$bump" "$COMMIT" -- "$CONTRACTS" >&2
    fail "$CONTRACTS at $COMMIT differs from the last published River UI ($bump). The app would carry a delegate or contract the live web UI does not. Publish the River UI first, then tag a commit at or after its bump."
fi

webapp_version="$(git show "$bump:published-contract/contract-version.txt" | tr -d '[:space:]')"
[[ -n "$webapp_version" ]] || fail "published-contract/contract-version.txt is empty at $bump."
echo "published web UI:      $webapp_version" >&2

# 4. Code hashes agree with the pointer records.
records="$(git show "$COMMIT:pointer-records.toml")" || fail "pointer-records.toml missing at $COMMIT."
# The code_hash of the one [[record]] whose wasm_path is $1. Fails unless exactly one
# record names that path and it has a code_hash.
record_hash() {
    awk -v want="$1" '
        function flush() { if (in_rec && path == want) { n++; if (hash != "") found = hash } }
        function value(line) { sub(/^[^"]*"/, "", line); sub(/".*$/, "", line); return line }
        /^[[:space:]]*\[/ {
            flush()
            in_rec = ($0 ~ /^[[:space:]]*\[\[record\]\][[:space:]]*$/); path = ""; hash = ""
            next
        }
        in_rec && /^[[:space:]]*wasm_path[[:space:]]*=/ { path = value($0) }
        in_rec && /^[[:space:]]*code_hash[[:space:]]*=/ { hash = value($0) }
        END { flush(); if (n != 1 || found == "") exit 1; print found }
    ' <<<"$records"
}
check_hash() {
    local path="$1" actual expected
    actual="$(git show "$COMMIT:$path" | "$B3SUM" --no-names | tr -d '[:space:]')"
    [[ -n "$actual" ]] || fail "Could not hash $path."
    expected="$(record_hash "$path")" || fail "pointer-records.toml has no single record with a code_hash for $path."
    echo "$path: $actual" >&2
    [[ "$actual" == "$expected" ]] \
        || fail "BLAKE3 of $path ($actual) does not match its pointer record ($expected)."
    printf '%s' "$actual"
}
delegate_hash="$(check_hash "$DELEGATE_WASM")"
contract_hash="$(check_hash "$CONTRACT_WASM")"

echo "version=$tag_version"
echo "webapp_version=$webapp_version"
echo "delegate_hash=$delegate_hash"
echo "contract_hash=$contract_hash"
