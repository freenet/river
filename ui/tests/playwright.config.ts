import { defineConfig, devices } from "@playwright/test";

// Engine-agnostic tests (usually hook-driven) tag themselves @chromium-only and run once.
const CHROMIUM_ONLY = /@chromium-only/;

export default defineConfig({
  testDir: ".",
  testMatch: "*.spec.ts",
  timeout: 60_000,
  retries: 2,
  use: {
    baseURL: process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082",
    navigationTimeout: 30_000,
    actionTimeout: 10_000,
    // Keep a trace of the FIRST attempt whenever a test has to be retried.
    //
    // `retries: 2` above means an intermittent failure is reported as "flaky"
    // and the run still goes green — so the evidence used to evaporate and all
    // anyone had left was the test's name. That is exactly how freenet/river#538
    // ended up with a plausible-but-wrong hypothesis attached to it: the theory
    // was a timeout under parallel load, but measuring the phases showed the
    // work finishing in ~1s against a 15s budget, so the real cause is still
    // unknown and no longer observable.
    //
    // `on-first-retry` costs nothing on a green run and nothing on a run with
    // no retries; when a flake does happen it leaves a full trace (DOM
    // snapshots, console, network) in test-results/, which CI uploads.
    trace: "on-first-retry",
  },
  projects: [
    // Desktop browsers
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
    {
      name: "firefox",
      grepInvert: CHROMIUM_ONLY,
      use: { ...devices["Desktop Firefox"] },
    },
    {
      name: "webkit",
      grepInvert: CHROMIUM_ONLY,
      use: { ...devices["Desktop Safari"] },
    },
    // A fractional device scale, so rows and scroll offsets land on fractional
    // CSS pixels. Only the geometry cases that opt in with @fractional-geometry;
    // they run on every other project too.
    {
      name: "chromium-dpr-1.5",
      testMatch: "conversation-autoscroll.spec.ts",
      grep: /@fractional-geometry/,
      grepInvert: CHROMIUM_ONLY,
      use: { ...devices["Desktop Chrome"], deviceScaleFactor: 1.5 },
    },
    // Mobile viewports (Chromium engine)
    {
      name: "mobile-chrome",
      grepInvert: CHROMIUM_ONLY,
      use: { ...devices["Pixel 5"] },
    },
    {
      name: "mobile-safari",
      grepInvert: CHROMIUM_ONLY,
      use: { ...devices["iPhone 13"] },
    },
  ],
  // Serve an already-built artifact statically; nothing here starts a server.
  //   cargo make build-ui-example-no-sync
  //   python3 -m http.server 8082 --bind 127.0.0.1 \
  //     --directory target/dx/river-ui/release/web/public
  // Not `dx serve`: it rebuilds and watches, so it can serve a stale build.
  // Port 8082 may already belong to another worktree's server; pick a free
  // port and set PLAYWRIGHT_BASE_URL (read above) to point at it.
});
