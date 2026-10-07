#!/usr/bin/env bash
#
# Remove the cached example UI output before an example/no-sync build.
#
# A cached index.html must not survive into that build: if dx decides there is
# nothing to do, the Playwright job would read the restored index from disk,
# serve that same index, and pass its disk-vs-HTTP bundle check without this
# build having produced anything. Deleting the selected public directory first
# means an index on disk afterwards can only have come from this dx run. Only
# that directory is removed; the Rust and Dioxus caches around it stay warm.
#
# Run from the repository root with BUILD_PROFILE set (cargo-make does both).
# Tested by scripts/tests/playwright-bundle-provenance-test.sh.

set -euo pipefail

: "${BUILD_PROFILE:?BUILD_PROFILE must be set}"
PUBLIC_DIR="target/dx/river-ui/${BUILD_PROFILE}/web/public"
rm -rf -- "$PUBLIC_DIR"
