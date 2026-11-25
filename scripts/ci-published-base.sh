#!/bin/bash
# Print the commit a PR's committed artifacts must be compared against in CI.
#
# Usage: scripts/ci-published-base.sh BASE_REF BASE_SHA HEAD_SHA
#
# The gates that guard a re-key ask "did this PR change a committed WASM, and
# is the OUTGOING generation registered?". The outgoing generation is the one
# that was PUBLISHED, i.e. what origin/main holds. For a PR whose base is main,
# that is the PR's base commit: unchanged behaviour. For a STACKED PR (base is
# another unmerged branch) the base commit is an unpublished chain tip, whose
# WASMs never shipped and are, rightly, not in the registries; comparing
# against it fails every stacked PR that rebuilds a WASM. So for a stacked PR
# we compare against `git merge-base origin/main HEAD`: the published lineage
# the whole stack branched from.
#
# If main moved after the stack branched, the merge-base is older than main's
# tip. A WASM that main changed since is not this PR's change, so it is not
# demanded here; the rebase onto the new main re-runs the gate against the
# newer lineage (and conflicts in the registry/WASM files surface there).
set -euo pipefail
BASE_REF="${1:?usage: $0 BASE_REF BASE_SHA HEAD_SHA}"
BASE_SHA="${2:?}"
HEAD_SHA="${3:?}"
if [ "$BASE_REF" = "main" ]; then
    echo "$BASE_SHA"
    exit 0
fi
git fetch --quiet origin main
git merge-base origin/main "$HEAD_SHA"
