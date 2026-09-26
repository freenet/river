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
  // The dev server must already be running:
  // cd ui && dx serve --port 8082 --features example-data,no-sync
});
