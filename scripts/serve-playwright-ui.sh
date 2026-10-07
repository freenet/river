#!/usr/bin/env bash
#
# Serve the release example UI just built and wait until the server answers
# with the same bundle the build wrote to disk. Run from the repository root
# after `cargo make build-ui-example-no-sync`.
#
# Serves the static output, not `dx serve`: the watcher can rebuild underneath
# the run or serve a stale binary, and then the suite measures something other
# than this commit. The build task deletes the restored public/ first, so an
# index here was written by this build; if dx wrote nothing, fail now rather
# than start a server on an empty directory. Matching disk and HTTP names alone
# cannot show that, since both would come from the same restored index.
#
# The server is left running in the background for the next CI step. Its
# address must match PLAYWRIGHT_BASE_URL in .github/workflows/build.yml.
# Tested by scripts/tests/playwright-bundle-provenance-test.sh.

set -euo pipefail

PUBLIC=target/dx/river-ui/release/web/public
if [ ! -s "$PUBLIC/index.html" ]; then
  echo "ERROR: the example UI build did not produce a nonempty $PUBLIC/index.html" >&2
  exit 1
fi
(cd "$PUBLIC" && python3 -m http.server 8082 --bind 127.0.0.1 &)
# `|| true`: an empty match, or a poll before the server listens, falls
# through to the retry and the diagnostic below instead of exiting here.
BUILT_JS=$(grep -o 'river-ui-dxh[0-9a-f]*\.js' "$PUBLIC/index.html" | head -1 || true)
for i in $(seq 1 30); do
  SERVED_JS=$(curl -s http://127.0.0.1:8082/ | grep -o 'river-ui-dxh[0-9a-f]*\.js' | head -1 || true)
  if [ -n "$SERVED_JS" ]; then break; fi
  sleep 1
done
echo "built: $BUILT_JS served: $SERVED_JS"
if [ -z "$BUILT_JS" ] || [ "$BUILT_JS" != "$SERVED_JS" ]; then
  echo "ERROR: the server is not serving this build"
  exit 1
fi
