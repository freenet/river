#!/usr/bin/env bash
#
# Tests for apps/desktop/packaging/verify-release.sh, the gate that decides
# whether a river-desktop-v* tag may be released (.github/workflows/release-desktop.yml).
#
# The gate only runs when a tag is pushed, so without this test nothing would
# exercise it before a release: a guard no job runs is a guard that passes by
# default (freenet/river#614). Each refusal below asserts BOTH a non-zero exit
# AND the gate's own message for that check, so deleting or weakening any one
# check makes its case fail here instead of passing silently.
#
# Builds a throwaway git repository with the files the gate reads. Needs git
# and python3 (3.11+, for tomllib); the BLAKE3 hasher is replaced by a SHA-256
# stand-in through the gate's B3SUM override, since what is tested is the
# comparison, not the hash function.
#
# Run: ./scripts/tests/verify-release-test.sh

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
gate="$repo_root/apps/desktop/packaging/verify-release.sh"

failures=0
checks=0
ok() { checks=$((checks + 1)); printf 'ok   %s\n' "$1"; }
fail() {
    checks=$((checks + 1)); failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"; shift
    for line in "$@"; do printf '       %s\n' "$line"; done
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# SHA-256 stand-in for b3sum, reading stdin like `b3sum --no-names`.
cat > "$work/fakeb3" <<'EOF'
#!/usr/bin/env bash
sha256sum | cut -d' ' -f1
EOF
chmod +x "$work/fakeb3"
export B3SUM="$work/fakeb3"
hash_of() { printf '%s' "$1" | sha256sum | cut -d' ' -f1; }

repo="$work/repo"
g() { git -C "$repo" "$@"; }
git init -q -b main "$repo"
g config user.email test@example.invalid
g config user.name test
g config commit.gpgsign false

write_state() { # <apps version> <delegate bytes> <contract bytes> [<record delegate bytes> <record contract bytes>]
    local version="$1" delegate="$2" contract="$3"
    local rec_delegate="${4:-$2}" rec_contract="${5:-$3}"
    mkdir -p "$repo/apps" "$repo/ui/public/contracts" "$repo/published-contract"
    printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\n' "$version" > "$repo/apps/Cargo.toml"
    printf '%s' "$delegate" > "$repo/ui/public/contracts/chat_delegate.wasm"
    printf '%s' "$contract" > "$repo/ui/public/contracts/room_contract.wasm"
    cat > "$repo/pointer-records.toml" <<EOF
[[record]]
wasm_path = "ui/public/contracts/room_contract.wasm"
code_hash = "$(hash_of "$rec_contract")"

[[record]]
wasm_path = "ui/public/contracts/chat_delegate.wasm"
code_hash = "$(hash_of "$rec_delegate")"
EOF
}
# Strictly increasing commit dates, so date order is well defined: without them
# every commit lands in the same second and `git log` order is arbitrary, which
# would let the first-parent case below pass by luck.
# The counter lives in a file because commit() usually runs in a $(...) subshell.
echo 1700000000 > "$work/tick"
next_date() {
    local tick=$(( $(cat "$work/tick") + 60 ))
    echo "$tick" > "$work/tick"
    export GIT_AUTHOR_DATE="@$tick" GIT_COMMITTER_DATE="@$tick"
}
commit() { next_date; g add -A && g commit -q -m "$1" && g rev-parse HEAD; }

# run <tag> <commit> [env assignments...] -> sets $out, $err, $rc
run() {
    local tag="$1" rev="$2"; shift 2
    out="$(cd "$repo" && env MAIN_REF=main "$@" "$gate" "$tag" "$rev" 2>"$work/err")"
    rc=$?
    err="$(cat "$work/err")"
}
expect_pass() { # <name>
    if [[ $rc -eq 0 ]]; then ok "$1"; else fail "$1" "exit $rc" "$err"; fi
}
expect_refusal() { # <name> <message fragment>
    if [[ $rc -eq 0 ]]; then
        fail "$1" "gate exited 0; expected a refusal containing: $2"
    elif ! grep -qF -- "$2" <<<"$err"; then
        fail "$1" "gate refused, but not with: $2" "$err"
    else
        ok "$1"
    fi
}

# --- History -----------------------------------------------------------------
# main:  c_init (no bump yet) -> c_pub (publish bump, web UI 7) -> c_ok
write_state 0.1.0 delegate-v1 contract-v1
echo 6 > "$repo/published-contract/contract-version.txt"
c_init="$(commit 'initial')"

# No publish bump on main yet.
run river-desktop-v0.1.0 "$c_init"
expect_refusal "refuses when main has no publish bump commit" "No '^chore(publish): bump webapp contract version' commit"

echo 7 > "$repo/published-contract/contract-version.txt"
c_pub="$(commit 'chore(publish): bump webapp contract version to 7 (#1)')"
echo doc > "$repo/README"
c_ok="$(commit 'docs: unrelated')"

# --- Positive ----------------------------------------------------------------
run river-desktop-v0.1.0 "$c_ok"
expect_pass "accepts a main commit whose WASM matches the last publish"
for want in "version=0.1.0" "webapp_version=7" \
            "delegate_hash=$(hash_of delegate-v1)" "contract_hash=$(hash_of contract-v1)"; do
    if grep -qxF "$want" <<<"$out"; then ok "outputs $want"; else fail "outputs $want" "$out"; fi
done

run river-desktop-v0.1.0 "$c_pub"
expect_pass "accepts the publish bump commit itself"

# --- Tag form and version ----------------------------------------------------
run river-desktop-v0.2.0 "$c_ok"
expect_refusal "refuses a tag whose version differs from apps/Cargo.toml" "does not match apps/Cargo.toml"
run riverctl-v0.1.0 "$c_ok"
expect_refusal "refuses a tag outside river-desktop-v*" "is not of the form river-desktop-v<version>"
run river-desktop-v "$c_ok"
expect_refusal "refuses a tag with no version" "does not end in a semver version"
run river-desktop-v0.1 "$c_ok"
expect_refusal "refuses a non-semver version" "does not end in a semver version"

# --- Not on main ---------------------------------------------------------------
g checkout -q -b feature
echo feature > "$repo/FEATURE"
c_branch="$(commit 'feature work')"
g checkout -q main
run river-desktop-v0.1.0 "$c_branch"
expect_refusal "refuses a commit that is not on main" "is not on main"
run river-desktop-v0.1.0 "$c_branch" DRY_RUN=1
expect_pass "DRY_RUN=1 skips only the main-ancestry check"

# --- WASM differs from the last publish ----------------------------------------
write_state 0.1.0 delegate-v2 contract-v1
c_rekey="$(commit 'feat: change the delegate (not yet published)')"
run river-desktop-v0.1.0 "$c_rekey"
expect_refusal "refuses when ui/public/contracts differs from the last publish bump" "differs from the last published River UI"
run river-desktop-v0.1.0 "$c_rekey" DRY_RUN=1
expect_refusal "DRY_RUN=1 still refuses unpublished WASM" "differs from the last published River UI"

# Publishing it makes the same tree releasable.
echo 8 > "$repo/published-contract/contract-version.txt"
c_pub2="$(commit 'chore(publish): bump webapp contract version to 8 (#2)')"
run river-desktop-v0.1.0 "$c_pub2"
expect_pass "accepts the re-keyed WASM once its publish bump is on main"
if grep -qxF "webapp_version=8" <<<"$out"; then ok "reports the newest publish"; else fail "reports the newest publish" "$out"; fi

# A bump reachable only through a merged side branch is not on main's first
# parent line, so it is not taken as main's latest publish, even though it is
# the newest bump by date.
g checkout -q -b side
echo 9 > "$repo/published-contract/contract-version.txt"
commit 'chore(publish): bump webapp contract version to 9 (#3)' >/dev/null
g checkout -q main
next_date
g merge -q --no-ff -m 'Merge side' side
c_merge="$(g rev-parse HEAD)"
run river-desktop-v0.1.0 "$c_merge"
if [[ $rc -eq 0 ]] && grep -qxF "webapp_version=8" <<<"$out"; then
    ok "finds the publish bump on the first-parent line only"
else
    fail "finds the publish bump on the first-parent line only" "exit $rc" "$out" "$err"
fi

# --- Pointer records disagree with the WASM ------------------------------------
write_state 0.1.0 delegate-v2 contract-v1 delegate-v2 contract-OTHER
commit 'chore: pointer record out of step' >/dev/null
# Make it the latest publish so only the hash check can refuse it.
echo 10 > "$repo/published-contract/contract-version.txt"
c_badrec_pub="$(commit 'chore(publish): bump webapp contract version to 10 (#4)')"
run river-desktop-v0.1.0 "$c_badrec_pub"
expect_refusal "refuses when a WASM hash differs from its pointer record" "does not match its pointer record"

# A missing record fails closed rather than skipping the comparison.
write_state 0.1.0 delegate-v2 contract-v1
printf '[[record]]\nwasm_path = "ui/public/contracts/room_contract.wasm"\ncode_hash = "%s"\n' \
    "$(hash_of contract-v1)" > "$repo/pointer-records.toml"
commit 'chore: drop the delegate pointer record' >/dev/null
echo 11 > "$repo/published-contract/contract-version.txt"
c_norec="$(commit 'chore(publish): bump webapp contract version to 11 (#5)')"
run river-desktop-v0.1.0 "$c_norec"
expect_refusal "refuses when a WASM has no pointer record" "has no single record with a code_hash for ui/public/contracts/chat_delegate.wasm"

printf '\n%d checks, %d failed\n' "$checks" "$failures"
[[ $failures -eq 0 ]]
