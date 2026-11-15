#!/bin/bash
# Print the tag of the same family released just before <tag>, or nothing if none.
#
#   scripts/previous-tag.sh <prefix> <tag>      e.g. previous-tag.sh riverctl-v riverctl-v0.2.26
#
# Used for `gh release ... --notes-start-tag` so generated release notes cover the
# changes since the previous release of the SAME artifact. This repo has several tag
# families (riverctl-v*, river-desktop-v*, the old bare v*), and GitHub's own choice
# of "previous" is the newest release of any family.
#
# Orders by version, not date, with versionsort.suffix=- so a prerelease sorts before
# its release (1.0.0-rc.1 < 1.0.0 < 1.0.1). Needs the tags fetched (fetch-depth: 0).

set -euo pipefail

PREFIX="${1:?usage: previous-tag.sh <prefix> <tag>}"
TAG="${2:?tag required}"

git -c versionsort.suffix=- tag --list "${PREFIX}*" --sort=v:refname \
    | awk -v t="$TAG" '$0 == t { print p; exit } { p = $0 }'
