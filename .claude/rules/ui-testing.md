---
description: Running the River web UI locally and testing it with Playwright (suite and MCP), including the ways a run silently tests the wrong build
globs:
  - ui/**
paths:
  - "ui/**"
---

# UI testing

## Run the UI locally

| Goal | Command | Serves |
|---|---|---|
| Quick iteration, no node | `cd ui && dx serve --port 8082 --features example-data,no-sync` | `127.0.0.1:8082`, rebuilds on save |
| Same, from another machine | add `--addr 0.0.0.0` | all interfaces |
| Built files for Playwright | `cargo make build-ui-example-no-sync`, then `cd target/dx/river-ui/release/web/public && python3 -m http.server 8082 &` | the release build, no rebuilds |
| Full stack on a local node | `./scripts/local-republish.sh` | prints `http://127.0.0.1:7510/v1/contract/web/{CONTRACT_ID}/` |

| Feature | Effect |
|---|---|
| `example-data` | Sample rooms, members, messages and reactions (enough to test the emoji picker) |
| `no-sync` | No Freenet connection, no WebSocket |

`dx serve` sometimes needs a manual restart. Check its output for build errors when the UI does not update.

## Playwright suite

**Run the suite before every publish to Freenet.** A republish takes minutes; a local run catches layout breaks first.

```bash
cargo make test-ui-playwright-setup        # once: install browsers
cargo make build-ui-example-no-sync
cd target/dx/river-ui/release/web/public && python3 -m http.server 8082 &
cd ui/tests && npx playwright test         # Chromium, Firefox, WebKit, mobile Chrome, mobile Safari
npx playwright test --project=webkit --grep "iframe"
```

| Viewport | What the suite checks |
|---|---|
| 1280px | 3-column layout, no overflow |
| 768px | Narrower sidebars via CSS `clamp` |
| 767px | Mobile mode, single panel |
| 480px | View switching: hamburger, members, back buttons |
| 320px | Small-screen readability |
| 1280px after mobile | Desktop layout recovers |
| Sandboxed iframe | Matches the Freenet gateway's embedding |

The suite in `ui/tests/` runs against a `dx build` with example data, in CI.

## Know which build you are testing

Three ways a run measures the wrong thing. All three hit in one session (2026-07-25).

| Trap | Symptom | Fix |
|---|---|---|
| `dx serve` serves a stale build | A mutation check (reintroduce the bug, expect red) "passes" because the watcher never rebuilt. The one test that proves a test is not vacuous gives the wrong answer | Use the static-server flow above. With `dx serve`, read a marker out of the DOM (a row's `class` is enough) before believing a pass or a fail |
| A stale `dx` earlier on `PATH` | `/opt/rust/bin/dx` was once 0.7.3 against dioxus 0.7.9. `dx serve` returned a 404 "dx is not serving a web app" page that looks like a broken change; `dx build` printed "incompatible" and built anyway | Every dx task depends on `check-dx-version`, which fails unless `dx --version` equals the dioxus version `Cargo.lock` resolves. Run `cargo make check-dx-version` by hand. Compare against `Cargo.lock`, not `ui/Cargo.toml` |
| Port 8082 is shared | It is the suite's default `baseURL`. Another worktree's server on 8082 means `npx playwright test` tests that build | Use a free port and set `PLAYWRIGHT_BASE_URL` |

## Playwright MCP

The Playwright MCP plugin is enabled in `.claude/settings.json`. The `local-dev` skill has the step-by-step flows. Use MCP for exploratory checks and debugging one issue. Use the suite for regressions across browsers and viewports.

## Tailwind

**`@source "../src/**/*.rs"` in `ui/assets/tailwind.css` is required.** Without it Tailwind v4 does not scan Rust files for class names, and responsive utilities like `md:flex` are never generated.
