import { test, expect, Page } from "@playwright/test";

// Regression coverage for freenet/river#509.
//
// #397 added loading / migrating / failed states for the rooms list, but put
// them entirely inside the rooms rail. Below the 768px breakpoint that panel is
// `display:none` rather than unmounted, and the default mobile view is Chat, so
// for the whole load window the only thing a phone user could see was the
// conversation panel's no-room screen — which rendered "Welcome to River /
// Create a new room, or get invited to an existing one" regardless of state.
//
// So mobile affirmatively told a mid-load user they had no rooms, and a FAILED
// load was pixel-identical to an empty account, with the Retry button invisible
// and the advice on screen being to create a room.
//
// The example build always seeds rooms, so these states are unreachable without
// a hook: `__riverTest.setRoomsLoadState(state)` clears ROOMS and sets
// ROOMS_LOAD_STATE (example_data.rs, gated on example-data + no-sync).

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function setLoadState(page: Page, state: string) {
  await page.evaluate((s) => {
    (window as any).__riverTest.setRoomsLoadState(s);
  }, state);
}

const WELCOME = "Welcome to River";

for (const { label, viewport, isMobile } of [
  // The mobile case is the reported bug…
  { label: "mobile", viewport: { width: 390, height: 844 }, isMobile: true },
  // …and the desktop centre panel showed the same false-empty text, so the
  // fix is viewport-independent and so is this coverage.
  { label: "desktop", viewport: { width: 1280, height: 800 }, isMobile: false },
]) {
  test.describe(`Rooms load state on the no-room screen (${label})`, () => {
    test.use({ viewport });

    test("a loading account is told its rooms are loading, not that it has none", async ({
      page,
    }) => {
      await page.goto("/");
      await waitForApp(page);

      // PREMISE, and the pre-fix state: the fixture seeds rooms but selects
      // none, so this screen opens on the Welcome copy. If that ever stops
      // being true, the "Welcome is gone" assertions below would pass without
      // the fix doing anything.
      await expect(page.getByText(WELCOME)).toBeVisible({ timeout: 10_000 });

      await setLoadState(page, "loading");

      // Scoped to THIS surface: the rail renders the same copy (deliberately —
      // both read one shared state), so an unscoped text locator is a strict-
      // mode violation rather than a signal.
      const loading = page.getByTestId("conversation-rooms-loading");
      await expect(loading).toBeVisible({ timeout: 5_000 });
      await expect(loading.getByText("Loading your rooms…")).toBeVisible();

      // The two surfaces read one shared state, so the rail must agree. This
      // is also the first browser coverage #397's rail states have ever had —
      // their absence is why #509 went unnoticed.
      await expect(page.getByTestId("room-list-loading")).toHaveCount(1);

      // The bug: the false-empty invitation to create a room.
      await expect(page.getByText(WELCOME)).toHaveCount(0);

      // The connection pill must SURVIVE this state, not be hidden by it.
      // A node that never connects leaves the load state at its `Loading`
      // default indefinitely — `begin_load_attempt`, which arms the 60s
      // backstop, only runs after a successful connect — so the pill is the
      // only thing on this screen that can tell the user the socket is down.
      //
      // Mobile only: on desktop the rooms rail carries its own copy, so this
      // would not be measuring the conversation panel's. Counting VISIBLE
      // matches, because below 768px the rail is `display:none` and its pill
      // is in the DOM either way.
      if (isMobile) {
        await expect(
          page.locator('[data-testid="connection-status-indicator"]:visible')
        ).toHaveCount(1);
      }

      // …and so must the #159 quickstart invite link: a brand-new user has no
      // rooms, so they are in THIS state for the whole load window, and it is
      // their only concrete next step.
      await expect(
        page.locator('a[href="https://freenet.org/quickstart#invite-form"]')
      ).toBeVisible();
    });

    test("a migrating account is told so", async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      // Settle on the Welcome screen first, so the hook is not racing
      // the fixture's own first render.
      await expect(page.getByText(WELCOME)).toBeVisible({ timeout: 10_000 });
      await setLoadState(page, "migrating");

      const migrating = page.getByTestId("conversation-rooms-migrating");
      await expect(migrating).toBeVisible({ timeout: 5_000 });
      await expect(migrating.getByText("Migrating your rooms…")).toBeVisible();
      await expect(page.getByTestId("room-list-migrating")).toHaveCount(1);
      await expect(page.getByText(WELCOME)).toHaveCount(0);
    });

    test("a failed load offers Retry instead of advice to create a room", async ({
      page,
    }) => {
      await page.goto("/");
      await waitForApp(page);
      // Settle on the Welcome screen first, so the hook is not racing
      // the fixture's own first render.
      await expect(page.getByText(WELCOME)).toBeVisible({ timeout: 10_000 });
      await setLoadState(page, "failed");

      const failed = page.getByTestId("conversation-rooms-error");
      await expect(failed).toBeVisible({ timeout: 5_000 });
      await expect(failed.getByText(/Couldn.t load your rooms/)).toBeVisible();
      await expect(page.getByTestId("room-list-error")).toHaveCount(1);
      await expect(
        page.getByTestId("conversation-rooms-retry-button")
      ).toBeVisible();
      await expect(page.getByText(WELCOME)).toHaveCount(0);

      // "Check your connection and try again" is only actionable if the
      // connection status is on the same screen.
      if (isMobile) {
        await expect(
          page.locator('[data-testid="connection-status-indicator"]:visible')
        ).toHaveCount(1);
      }
    });

    test("a genuinely empty account still gets the Welcome screen", async ({
      page,
    }) => {
      await page.goto("/");
      await waitForApp(page);
      await expect(page.getByText(WELCOME)).toBeVisible({ timeout: 10_000 });

      // Drive a REAL transition. Asserting "loaded" straight from the fixture's
      // own start state would be satisfied by the pre-hook DOM: Welcome is
      // already visible and neither state element exists, so the assertions
      // would resolve against the old render and pass even if the hook did
      // nothing (it logs and returns on an unrecognised string) or if the
      // Empty arm were broken.
      await setLoadState(page, "loading");
      await expect(page.getByText(WELCOME)).toHaveCount(0, { timeout: 5_000 });

      await setLoadState(page, "loaded");

      // The one state that SHOULD say "create a room": the load resolved and
      // there really is nothing. `room-list-empty` proves BOTH halves of the
      // premise — the state reached Loaded and ROOMS really was cleared —
      // which `WELCOME` alone cannot, since it also renders for `List`.
      await expect(page.getByText(WELCOME)).toBeVisible({ timeout: 5_000 });
      await expect(page.getByTestId("room-list-empty")).toHaveCount(1);
      await expect(
        page.getByTestId("conversation-rooms-loading")
      ).toHaveCount(0);
      await expect(page.getByTestId("conversation-rooms-error")).toHaveCount(0);
    });
  });
}
