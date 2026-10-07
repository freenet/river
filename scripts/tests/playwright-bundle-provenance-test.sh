#!/usr/bin/env bash
#
# Tests that the Playwright job serves output produced by THIS build, not
# output restored from the `target/` cache.
#
# The `ui-playwright-tests` job restores all of `target/`, builds with
# `cargo make build-ui-example-no-sync`, and then compares the bundle name in
# the index on disk with the one served over HTTP. Both names can come from the
# same restored index if the build leaves it untouched, so the comparison alone
# proves agreement, not provenance. The fix deletes the selected `public`
# directory before every example build; this file checks that deletion, the
# readiness gate that rejects missing output, and the wiring between them.
#
# The production scripts scripts/clean-ui-example-output.sh and
# scripts/serve-playwright-ui.sh are executed against temporary
# repository-shaped fixtures, with `python3`, `curl` and `sleep` stubbed. No
# port, node, Rust build or browser is needed. A few wiring assertions then
# check that the build and CI actually invoke those scripts.
#
# Run: ./scripts/tests/playwright-bundle-provenance-test.sh
# Point it at other copies (for mutation checks) with PROVENANCE_CLEANUP,
# PROVENANCE_SERVE, PROVENANCE_MAKEFILE and PROVENANCE_WORKFLOW.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cleanup_script="${PROVENANCE_CLEANUP:-$repo_root/scripts/clean-ui-example-output.sh}"
serve_script="${PROVENANCE_SERVE:-$repo_root/scripts/serve-playwright-ui.sh}"
makefile="${PROVENANCE_MAKEFILE:-$repo_root/Makefile.toml}"
workflow="${PROVENANCE_WORKFLOW:-$repo_root/.github/workflows/build.yml}"

failures=0
checks=0

ok() {
    checks=$((checks + 1))
    printf 'ok   %s\n' "$1"
}

fail() {
    checks=$((checks + 1))
    failures=$((failures + 1))
    printf 'FAIL %s\n' "$1"
    shift
    for line in "$@"; do printf '       %s\n' "$line"; done
}

# ---------------------------------------------------------------------------
# Block scoping for the wiring assertions. Each helper exits 3 unless it finds
# exactly one match, so a renamed or deleted block is reported instead of
# matching zero sites and passing.
# ---------------------------------------------------------------------------

# Lines of the single cargo-make task `$2`, up to the next `[section]`.
toml_task_block() {
    awk -v hdr="[tasks.$2]" '
        $0 == hdr { n++; inblk = 1; next }
        inblk && /^\[/ { inblk = 0 }
        inblk { print }
        END { if (n != 1) exit 3 }
    ' "$1"
}

# Lines of the single workflow job `$2`, up to the next job or top-level key.
workflow_job_block() {
    awk -v job="  $2:" '
        $0 == job { n++; inb = 1; next }
        inb && /^  [^ #][^:]*:/ { inb = 0 }
        inb && /^[^ #]/ { inb = 0 }
        inb { print }
        END { if (n != 1) exit 3 }
    ' "$1"
}

for script in "$cleanup_script" "$serve_script"; do
    if [[ ! -f "$script" ]]; then
        printf 'FAIL missing script %s\n\nCannot run the cases without it; stopping.\n' "$script"
        exit 1
    fi
done
# Resolve to absolute paths: the cases run from inside fixtures.
cleanup_script="$(cd "$(dirname "$cleanup_script")" && pwd)/$(basename "$cleanup_script")"
serve_script="$(cd "$(dirname "$serve_script")" && pwd)/$(basename "$serve_script")"

job_block=""
if job_block="$(workflow_job_block "$workflow" ui-playwright-tests)"; then
    ok "build.yml has exactly one ui-playwright-tests job"
else
    printf 'FAIL build.yml has exactly one ui-playwright-tests job in %s\n' "$workflow"
    printf '\nCannot check the CI wiring without it; stopping.\n'
    exit 1
fi

# ---------------------------------------------------------------------------
# Fixtures.
# ---------------------------------------------------------------------------

sandbox="$(mktemp -d)"
trap 'chmod -R u+w "$sandbox" 2>/dev/null; rm -rf "$sandbox"' EXIT

stubs="$sandbox/bin"
mkdir -p "$stubs"

# The static server: record that it was started and exit at once.
cat >"$stubs/python3" <<'STUB'
#!/usr/bin/env bash
touch "$STUB_SERVER_MARKER"
exit 0
STUB

# HTTP: answer with whatever the case put in $STUB_SERVED_HTML, after failing
# the first $STUB_CURL_REFUSALS polls the way curl does before a server listens.
cat >"$stubs/curl" <<'STUB'
#!/usr/bin/env bash
refusals="$(cat "$STUB_CURL_REFUSALS" 2>/dev/null || echo 0)"
if [[ "$refusals" -gt 0 ]]; then
    echo $((refusals - 1)) >"$STUB_CURL_REFUSALS"
    exit 7
fi
cat "$STUB_SERVED_HTML" 2>/dev/null
exit 0
STUB

# The readiness loop sleeps between polls; a case that never serves a bundle
# would otherwise take 30 seconds.
cat >"$stubs/sleep" <<'STUB'
#!/usr/bin/env bash
exit 0
STUB
chmod +x "$stubs/python3" "$stubs/curl" "$stubs/sleep"

export STUB_SERVER_MARKER="$sandbox/server-started"
export STUB_SERVED_HTML="$sandbox/served.html"
export STUB_CURL_REFUSALS="$sandbox/curl-refusals"

OLD_JS="river-ui-dxh25b5ff1a9ac3ccf1.js"
NEW_JS="river-ui-dxh0123456789abcdef.js"

fixture_count=0
public_rel="target/dx/river-ui/release/web/public"

index_html() {
    printf '<!DOCTYPE html><html><head><script src="/./wasm/%s"></script></head></html>\n' "$1"
}

# A repository-shaped directory holding a cached build: an index, a bundle and
# assets in the selected `public`, plus dependency-cache sentinels elsewhere
# under `target/` that cleanup must leave alone.
new_fixture() {
    fixture_count=$((fixture_count + 1))
    fixture="$sandbox/repo-$fixture_count"
    mkdir -p "$fixture/$public_rel/assets" "$fixture/$public_rel/wasm" \
        "$fixture/target/release/deps" \
        "$fixture/target/wasm32-unknown-unknown/release/incremental" \
        "$fixture/target/dx/river-ui/debug/web/public"
    index_html "$OLD_JS" >"$fixture/$public_rel/index.html"
    echo 'bundle' >"$fixture/$public_rel/wasm/$OLD_JS"
    echo 'css' >"$fixture/$public_rel/assets/tailwind-dxh00.css"
    echo 'dep' >"$fixture/target/release/deps/libsentinel.rlib"
    echo 'inc' >"$fixture/target/wasm32-unknown-unknown/release/incremental/sentinel"
    echo 'other profile' >"$fixture/target/dx/river-ui/debug/web/public/index.html"
    echo 'sibling' >"$fixture/target/dx/river-ui/release/web/sibling-sentinel"
}

# cargo-make runs the script from the repository root.
run_cleanup() {
    (cd "$1" && BUILD_PROFILE="${2-release}" bash "$cleanup_script") >"$sandbox/cleanup.out" 2>&1
}

# A successful build that writes a fresh index naming bundle $2.
simulate_rebuild() {
    mkdir -p "$1/$public_rel/wasm"
    index_html "$2" >"$1/$public_rel/index.html"
    echo 'bundle' >"$1/$public_rel/wasm/$2"
}

serve() {
    index_html "$1" >"$STUB_SERVED_HTML"
}

run_readiness() {
    rm -f "$STUB_SERVER_MARKER"
    out="$(cd "$1" && PATH="$stubs:$PATH" bash "$serve_script" 2>&1)"
    status=$?
    # The server is started in the background; give the stub time to land.
    sleep 0.2
    server_started=0
    [[ -e "$STUB_SERVER_MARKER" ]] && server_started=1
}

sentinels_intact() {
    [[ -f "$1/target/release/deps/libsentinel.rlib" &&
        -f "$1/target/wasm32-unknown-unknown/release/incremental/sentinel" &&
        -f "$1/target/dx/river-ui/debug/web/public/index.html" &&
        -f "$1/target/dx/river-ui/release/web/sibling-sentinel" ]]
}

# ---------------------------------------------------------------------------
# 1. The original false positive: cached index, matching HTTP, no-op build.
# ---------------------------------------------------------------------------

new_fixture
run_cleanup "$fixture"
# The no-op build: dx exits 0 without writing anything.
serve "$OLD_JS"
run_readiness "$fixture"
if [[ "$status" -ne 0 && "$server_started" -eq 0 ]]; then
    ok "cached output plus a no-op build fails readiness even when names match"
else
    fail "cached output plus a no-op build fails readiness even when names match" \
        "exited $status, server started: $server_started" "$out"
fi

# ---------------------------------------------------------------------------
# 2. Cleanup removes the selected public directory and nothing else.
# ---------------------------------------------------------------------------

new_fixture
if run_cleanup "$fixture" && [[ ! -e "$fixture/$public_rel" ]] && sentinels_intact "$fixture"; then
    ok "cleanup removes the selected public directory and keeps the rest of target/"
else
    fail "cleanup removes the selected public directory and keeps the rest of target/" \
        "$(find "$fixture/target" -type f | sed "s|$fixture/||" | sort | tr '\n' ' ')" \
        "$(cat "$sandbox/cleanup.out")"
fi

new_fixture
rm -rf "${fixture:?}/$public_rel"
if run_cleanup "$fixture" && sentinels_intact "$fixture"; then
    ok "cleanup succeeds when public is already absent"
else
    fail "cleanup succeeds when public is already absent" "$(cat "$sandbox/cleanup.out")"
fi

new_fixture
if ! run_cleanup "$fixture" "" && [[ -f "$fixture/$public_rel/index.html" ]] && sentinels_intact "$fixture"; then
    ok "cleanup refuses to run without BUILD_PROFILE"
else
    fail "cleanup refuses to run without BUILD_PROFILE" \
        "an empty profile would select target/dx/river-ui//web/public" \
        "$(cat "$sandbox/cleanup.out")"
fi

# A removal error must fail the task, so cargo-make stops before dx runs.
# Root ignores directory permissions, so this case cannot be staged there.
if [[ "$(id -u)" -eq 0 ]]; then
    ok "cleanup fails when removal fails (skipped: running as root)"
else
    new_fixture
    chmod a-w "$fixture/$public_rel" "$fixture/target/dx/river-ui/release/web"
    if ! run_cleanup "$fixture"; then
        ok "cleanup fails when removal fails"
    else
        fail "cleanup fails when removal fails" \
            "exited 0 with $public_rel still present" "$(cat "$sandbox/cleanup.out")"
    fi
    chmod u+w "$fixture/$public_rel" "$fixture/target/dx/river-ui/release/web"
fi

# ---------------------------------------------------------------------------
# 3. A fresh build passes, including when its hash equals the cached one.
# ---------------------------------------------------------------------------

for js in "$NEW_JS" "$OLD_JS"; do
    label="fresh output matching HTTP passes readiness"
    [[ "$js" == "$OLD_JS" ]] && label="$label when the new hash equals the cached one"
    new_fixture
    run_cleanup "$fixture"
    simulate_rebuild "$fixture" "$js"
    serve "$js"
    run_readiness "$fixture"
    if [[ "$status" -eq 0 && "$out" == *"built: $js served: $js"* ]]; then
        ok "$label"
    else
        fail "$label" "exited $status" "$out"
    fi
done

# The server is started in the background, so the first polls can land
# before it listens. The loop has to retry them, not exit on them.
new_fixture
run_cleanup "$fixture"
simulate_rebuild "$fixture" "$NEW_JS"
serve "$NEW_JS"
echo 2 >"$STUB_CURL_REFUSALS"
run_readiness "$fixture"
if [[ "$status" -eq 0 && "$out" == *"built: $NEW_JS served: $NEW_JS"* ]]; then
    ok "readiness retries polls made before the server listens"
else
    fail "readiness retries polls made before the server listens" "exited $status" "$out"
fi
rm -f "$STUB_CURL_REFUSALS"

# ---------------------------------------------------------------------------
# 4. A different served bundle still fails the disk-vs-HTTP comparison.
# ---------------------------------------------------------------------------

new_fixture
run_cleanup "$fixture"
simulate_rebuild "$fixture" "$NEW_JS"
serve "$OLD_JS"
run_readiness "$fixture"
if [[ "$status" -ne 0 && "$out" == *"not serving this build"* ]]; then
    ok "fresh output with a different served bundle fails readiness"
else
    fail "fresh output with a different served bundle fails readiness" "exited $status" "$out"
fi

# ---------------------------------------------------------------------------
# 5. Missing, empty or bundle-less output fails; missing and empty output are
#    rejected before the server starts.
# ---------------------------------------------------------------------------

for case in "a missing" "an empty"; do
    new_fixture
    if [[ "$case" == "a missing" ]]; then
        rm -f "$fixture/$public_rel/index.html"
    else
        : >"$fixture/$public_rel/index.html"
    fi
    serve "$OLD_JS"
    run_readiness "$fixture"
    if [[ "$status" -ne 0 && "$server_started" -eq 0 ]]; then
        ok "$case index fails before the server starts"
    else
        fail "$case index fails before the server starts" \
            "exited $status, server started: $server_started" "$out"
    fi
done

new_fixture
echo '<!DOCTYPE html><html><head></head></html>' >"$fixture/$public_rel/index.html"
serve "$OLD_JS"
run_readiness "$fixture"
if [[ "$status" -ne 0 ]]; then
    ok "an index with no river-ui bundle reference fails readiness"
else
    fail "an index with no river-ui bundle reference fails readiness" "exited 0" "$out"
fi

# ---------------------------------------------------------------------------
# 6. Call sites. The scripts above prove nothing if the build and CI no longer
#    run them, so pin the wiring. Comment lines are stripped before matching.
# ---------------------------------------------------------------------------

strip_comments() { grep -v '^[[:space:]]*#'; }

if build_block="$(toml_task_block "$makefile" build-ui-example-no-sync)"; then
    build_block="$(printf '%s\n' "$build_block" | strip_comments)"
    deps="$(printf '%s\n' "$build_block" | grep '^dependencies = ' || true)"
    missing=()
    for dep in build-chat-delegate build-tailwind clean-ui-example-output touch-ui-files; do
        [[ "$deps" == *"\"$dep\""* ]] || missing+=("$dep")
    done
    if [[ "$(printf '%s\n' "$deps" | grep -c .)" -eq 1 && ${#missing[@]} -eq 0 ]]; then
        ok "build-ui-example-no-sync depends on delegate, Tailwind, cleanup and touch"
    else
        fail "build-ui-example-no-sync depends on delegate, Tailwind, cleanup and touch" \
            "dependencies: ${deps:-<none>}" "missing: ${missing[*]:-}"
    fi

    if printf '%s\n' "$build_block" | grep -qx 'command = "dx"' &&
        printf '%s\n' "$build_block" | grep -qx 'cwd = "./ui"' &&
        printf '%s\n' "$build_block" | grep -q 'UI_FEATURES = "example-data,no-sync"'; then
        ok "build-ui-example-no-sync still runs dx from ui/ with example-data,no-sync"
    else
        fail "build-ui-example-no-sync still runs dx from ui/ with example-data,no-sync" \
            "$build_block"
    fi
else
    fail "Makefile.toml has exactly one build-ui-example-no-sync task"
fi

if clean_block="$(toml_task_block "$makefile" clean-ui-example-output)" &&
    clean_code="$(printf '%s\n' "$clean_block" | strip_comments)" &&
    printf '%s\n' "$clean_code" | grep -qx 'command = "bash"' &&
    printf '%s\n' "$clean_code" | grep -qx 'args = \["scripts/clean-ui-example-output\.sh"\]'; then
    ok "clean-ui-example-output runs scripts/clean-ui-example-output.sh"
else
    fail "clean-ui-example-output runs scripts/clean-ui-example-output.sh" \
        "${clean_block:-<task not found>}"
fi

if touch_block="$(toml_task_block "$makefile" touch-ui-files)" &&
    printf '%s\n' "$touch_block" | strip_comments | grep -qx 'touch ui/src/main.rs'; then
    ok "touch-ui-files still touches ui/src/main.rs"
else
    fail "touch-ui-files still touches ui/src/main.rs" "${touch_block:-<task not found>}"
fi

job_code="$(printf '%s\n' "$job_block" | strip_comments)"
build_line="$(printf '%s\n' "$job_code" | grep -n 'run: cargo make build-ui-example-no-sync$' || true)"
if [[ "$(printf '%s\n' "$build_line" | grep -c .)" -eq 1 ]]; then
    ok "ui-playwright-tests builds through cargo make build-ui-example-no-sync"
else
    fail "ui-playwright-tests builds through cargo make build-ui-example-no-sync" \
        "matches: ${build_line:-<none>}"
fi

guard_line="$(printf '%s\n' "$job_code" |
    grep -n 'run: \./scripts/tests/playwright-bundle-provenance-test\.sh$' || true)"
if [[ "$(printf '%s\n' "$guard_line" | grep -c .)" -eq 1 && -n "$build_line" &&
    "${guard_line%%:*}" -lt "${build_line%%:*}" ]]; then
    ok "ui-playwright-tests runs this test once, before the build"
else
    fail "ui-playwright-tests runs this test once, before the build" \
        "test invocation: ${guard_line:-<none>}" "build: ${build_line:-<none>}"
fi

serve_line="$(printf '%s\n' "$job_code" | grep -n 'run: \./scripts/serve-playwright-ui\.sh$' || true)"
if [[ "$(printf '%s\n' "$serve_line" | grep -c .)" -eq 1 && -n "$build_line" &&
    "${serve_line%%:*}" -gt "${build_line%%:*}" ]]; then
    ok "ui-playwright-tests runs the readiness script once, after the build"
else
    fail "ui-playwright-tests runs the readiness script once, after the build" \
        "readiness: ${serve_line:-<none>}" "build: ${build_line:-<none>}"
fi

# Playwright reads PLAYWRIGHT_BASE_URL (ui/tests/playwright.config.ts), so the
# script's bind address and port have to be the one the workflow hands it.
serve_code="$(strip_comments <"$serve_script")"
if [[ "$serve_code" == *"--bind 127.0.0.1"* && "$serve_code" == *"http.server 8082 "* &&
    "$serve_code" == *"http://127.0.0.1:8082/"* ]] &&
    printf '%s\n' "$job_code" | grep -q 'PLAYWRIGHT_BASE_URL: http://127.0.0.1:8082$'; then
    ok "the server binds 127.0.0.1:8082 and Playwright targets it explicitly"
else
    fail "the server binds 127.0.0.1:8082 and Playwright targets it explicitly"
fi

printf '\n%d checks, %d failures\n' "$checks" "$failures"
[[ "$failures" -eq 0 ]]
