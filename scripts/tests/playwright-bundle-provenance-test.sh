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
# The cleanup and readiness bodies are extracted from Makefile.toml and
# build.yml and executed against temporary fixtures, with `python3`, `curl`
# and `sleep` stubbed. No port, node, Rust build or browser is needed.
#
# Run: ./scripts/tests/playwright-bundle-provenance-test.sh
# Point it at other copies (for mutation checks) with PROVENANCE_MAKEFILE and
# PROVENANCE_WORKFLOW.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
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
# Extraction. Each helper exits 3 unless it finds exactly one match, so a
# renamed or deleted block is reported instead of matching zero sites and
# passing.
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

# The literal `script = '''...'''` body of a task block on stdin.
toml_script_body() {
    awk -v opener="script = '''" -v closer="'''" '
        !inb && $0 == opener { n++; inb = 1; next }
        inb && $0 == closer { inb = 0; next }
        inb { print }
        END { if (n != 1 || inb) exit 3 }
    '
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

# The `run: |` body of the single step named `$1` in a job block on stdin,
# with the block indentation removed.
workflow_step_run() {
    awk -v name="- name: $1" '
        function indent(s) { match(s, /^ */); return RLENGTH }
        {
            ind = indent($0)
            blank = ($0 ~ /^[ \t]*$/)
            trimmed = substr($0, ind + 1)
            if (inrun) {
                if (blank) { pending = pending "\n"; next }
                if (ind > runind) {
                    if (bodyind < 0) bodyind = ind
                    out = out pending substr($0, bodyind + 1) "\n"
                    pending = ""
                    next
                }
                inrun = 0
            }
            if (instep && !blank && ind <= stepind) instep = 0
            if (trimmed == name) { n++; instep = 1; stepind = ind; next }
            if (instep && trimmed == "run: |") {
                runs++; inrun = 1; runind = ind; bodyind = -1; pending = ""
            }
        }
        END {
            if (n != 1 || runs != 1 || out == "") exit 3
            printf "%s", out
        }
    '
}

extract_failed=0

cleanup_body=""
if block="$(toml_task_block "$makefile" clean-ui-example-output)" &&
    cleanup_body="$(printf '%s\n' "$block" | toml_script_body)" &&
    [[ -n "$cleanup_body" ]]; then
    ok "Makefile.toml has exactly one clean-ui-example-output script"
else
    cleanup_body=""
    extract_failed=1
    fail "Makefile.toml has exactly one clean-ui-example-output script" \
        "no single [tasks.clean-ui-example-output] with a script = ''' block in $makefile" \
        "the cases below run with an empty preparation step"
fi

job_block=""
readiness_body=""
if job_block="$(workflow_job_block "$workflow" ui-playwright-tests)" &&
    readiness_body="$(printf '%s\n' "$job_block" |
        workflow_step_run "Serve the built UI and wait for readiness")" &&
    [[ -n "$readiness_body" ]]; then
    ok "build.yml has exactly one readiness step in ui-playwright-tests"
else
    printf 'FAIL build.yml has exactly one readiness step in ui-playwright-tests\n'
    printf '       no single "Serve the built UI and wait for readiness" run: | block in %s\n' "$workflow"
    printf '\nCannot exercise readiness without its body; stopping.\n'
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

# HTTP: answer with whatever the case put in $STUB_SERVED_HTML.
cat >"$stubs/curl" <<'STUB'
#!/usr/bin/env bash
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

# cargo-make runs script blocks with `sh`, from the repository root.
run_cleanup() {
    (cd "$1" && BUILD_PROFILE="${2-release}" sh -c "$cleanup_body") >"$sandbox/cleanup.out" 2>&1
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

# GitHub Actions runs `run:` blocks with `bash -e -o pipefail`.
run_readiness() {
    rm -f "$STUB_SERVER_MARKER"
    out="$(cd "$1" && PATH="$stubs:$PATH" bash -e -o pipefail -c "$readiness_body" 2>&1)"
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
if [[ "$status" -ne 0 ]]; then
    ok "cached output plus a no-op build fails readiness even when names match"
else
    fail "cached output plus a no-op build fails readiness even when names match" \
        "readiness exited 0 on the restored index" "$out"
fi

# ---------------------------------------------------------------------------
# 2. Cleanup removes the selected public directory and nothing else.
# ---------------------------------------------------------------------------

if [[ -n "$cleanup_body" ]]; then
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

new_fixture
rm -f "$fixture/$public_rel/index.html"
serve "$OLD_JS"
run_readiness "$fixture"
if [[ "$status" -ne 0 && "$server_started" -eq 0 ]]; then
    ok "a missing index fails before the server starts"
else
    fail "a missing index fails before the server starts" \
        "exited $status, server started: $server_started" "$out"
fi

new_fixture
: >"$fixture/$public_rel/index.html"
serve "$OLD_JS"
run_readiness "$fixture"
if [[ "$status" -ne 0 && "$server_started" -eq 0 ]]; then
    ok "an empty index fails before the server starts"
else
    fail "an empty index fails before the server starts" \
        "exited $status, server started: $server_started" "$out"
fi

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
# 6. Preparation runs on every build, not only on a cache miss.
# ---------------------------------------------------------------------------

new_fixture
rm -rf "${fixture:?}/$public_rel"
run_cleanup "$fixture"
simulate_rebuild "$fixture" "$NEW_JS"
serve "$NEW_JS"
run_readiness "$fixture"
first_status=$status
run_cleanup "$fixture"
run_readiness "$fixture"
if [[ "$first_status" -eq 0 && "$status" -ne 0 ]]; then
    ok "a second, warm no-op build fails readiness after a good first run"
else
    fail "a second, warm no-op build fails readiness after a good first run" \
        "first run exited $first_status, second exited $status" "$out"
fi

# ---------------------------------------------------------------------------
# 7. Call sites. The bodies above prove nothing if the build no longer runs
#    them, so pin the wiring. Comment lines are stripped before matching.
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

if [[ "$readiness_body" == *"--bind 127.0.0.1"* && "$readiness_body" == *"http://127.0.0.1:8082/"* ]] &&
    printf '%s\n' "$job_code" | grep -q 'PLAYWRIGHT_BASE_URL: http://127.0.0.1:8082$'; then
    ok "the server binds 127.0.0.1:8082 and Playwright targets it explicitly"
else
    fail "the server binds 127.0.0.1:8082 and Playwright targets it explicitly"
fi

[[ "$extract_failed" -eq 0 ]] || printf '\nnote: clean-ui-example-output could not be extracted\n'
printf '\n%d checks, %d failures\n' "$checks" "$failures"
[[ "$failures" -eq 0 ]]
