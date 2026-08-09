#!/usr/bin/env bash
#
# Fail if any workspace member has no `cargo test` step in build.yml.
#
# Why this exists (freenet/river#614): a test that no CI job runs is
# indistinguishable from a test that passes. This repo has hit that failure at
# least five times — river-ui's unit tests, riverctl's unit tests, the bulk of
# river-core's lib tests, the nine common/tests files that an allowlist of
# `--test <name>` steps silently skipped, and finally the chat-delegate's 40
# tests, which never ran from the crate's creation until #614. Each time the
# fix was "add the missing step", which fixes the instance and not the class.
#
# The class fix is this script: adding a workspace member without wiring its
# tests into CI now fails CI. See the long-form comments in
# .github/workflows/build.yml for the individual incidents.
#
# A member with genuinely no tests still needs a step — `cargo test -p <name>`
# on a testless crate is fast and passes, and it means the day someone adds the
# first test to that crate, it runs.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workflow="$repo_root/.github/workflows/build.yml"
root_manifest="$repo_root/Cargo.toml"

[[ -f "$workflow" ]] || { echo "ERROR: $workflow not found" >&2; exit 1; }
[[ -f "$root_manifest" ]] || { echo "ERROR: $root_manifest not found" >&2; exit 1; }

# `run:` lines that invoke cargo test. Restricting to these means a package
# named only in a build step or a comment does not count as covered.
test_invocations="$(grep -E '^\s*(run:|-)?\s*cargo test ' "$workflow" || true)"

if [[ -z "$test_invocations" ]]; then
  echo "ERROR: build.yml contains no 'cargo test' invocations at all." >&2
  exit 1
fi

members="$(sed -n '/^members = \[/,/^]/p' "$root_manifest" \
  | grep -oE '"[^"]+"' | tr -d '"')"

if [[ -z "$members" ]]; then
  echo "ERROR: could not parse [workspace] members from $root_manifest" >&2
  exit 1
fi

missing=0
for member in $members; do
  manifest="$repo_root/$member/Cargo.toml"
  if [[ ! -f "$manifest" ]]; then
    echo "ERROR: workspace member '$member' has no Cargo.toml" >&2
    missing=1
    continue
  fi

  pkg="$(grep -m1 -E '^name\s*=' "$manifest" | sed -E 's/^name\s*=\s*"(.*)".*/\1/')"
  if [[ -z "$pkg" ]]; then
    echo "ERROR: could not read package name from $manifest" >&2
    missing=1
    continue
  fi

  # Match `-p <pkg>` or `--package <pkg>`, requiring a word boundary so
  # `-p web-container-contract` does not satisfy `web-container-tool`.
  if grep -qE "(-p|--package)[= ]$pkg([[:space:]]|$)" <<<"$test_invocations"; then
    echo "ok:      $pkg ($member)"
  else
    echo "MISSING: $pkg ($member) has no 'cargo test -p $pkg' step in build.yml" >&2
    missing=1
  fi
done

if (( missing )); then
  cat >&2 <<'EOF'

Every workspace member must have a `cargo test` step in
.github/workflows/build.yml. Add one next to the existing per-crate steps:

    - name: Test <crate>
      env:
        RUST_MIN_STACK: 8388608
        CARGO_TARGET_DIR: ${{ github.workspace }}/target
      run: cargo test -p <crate>

Do not silence this check by deleting the member from the list it reads —
it reads [workspace] members directly, which is the point (freenet/river#614).
EOF
  exit 1
fi

echo "All workspace members have a cargo test step in build.yml."
