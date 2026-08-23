import { test, expect, Page } from "@playwright/test";

// Regression coverage for the hidden-tab title's unread badge
// (freenet/river#446 and its predecessor bug).
//
// Original report (Ian Clarke, 2026-05-06): the title incorrectly showed
// "(N) River - …" with a non-zero unread count even though the user was
// just active on the page. The original fix for that made
// `on_visibility_change` mark EVERY room as read on the visible -> hidden
// transition, so anything already on screen anywhere stopped counting.
//
// That fix was itself the freenet/river#446 bug: sweeping every room read
// on a mere tab-hide silently erased unread state for rooms the user never
// even opened — routine on mobile, where backgrounding the app fires
// `visibilitychange` constantly. The current fix marks ONLY the room the
// user was actually looking at. This changes the hidden-tab title's
// semantics from "unread since hide" to "accumulated unread across all
// rooms" — the same number the room-list and hamburger badges already show.
//
// The example-data fixture ships with pre-existing unread messages in
// "Public Discussion Room" and "Team Chat Room" (the local user is only an
// observer there, so every message counts as unread until a room is
// opened) — these tests rely on that, not on a zero-unread starting state.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function openRoom(page: Page, name: string) {
  const roomBtn = page.getByRole("button", { name });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name })).toBeVisible({
    timeout: 5_000,
  });
}

// Force the tab into the "hidden" visibility state. We override the
// `document.hidden` and `document.visibilityState` getters and dispatch the
// `visibilitychange` event the same way Chromium / WebKit / Firefox do when
// the tab goes to the background.
async function setTabHidden(page: Page) {
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => true,
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "hidden",
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

async function setTabVisible(page: Page) {
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => false,
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

test.describe("Document title unread badge", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  // freenet/river#446's headline behavioral change. Opening "Public
  // Discussion Room" marks IT read, but "Team Chat Room" — never opened,
  // every message authored by someone else — stays unread. Under the OLD
  // `mark_all_rooms_as_read` behavior, hiding the tab would have swept Team
  // Chat Room read too, leaving the title badge-free. The fix marks only
  // the current room, so Team Chat Room's unread messages now surface as an
  // accumulated "(N)" count the moment the tab hides.
  test("hiding the tab surfaces accumulated unread from a room that was never opened", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await openRoom(page, "Public Discussion Room");
    await expect(page).toHaveTitle("River - Public Discussion Room", {
      timeout: 5_000,
    });

    // Confirm "Team Chat Room" genuinely still has unread messages before
    // hiding — otherwise this test would prove nothing about the hide path.
    const teamChatBtn = page.getByRole("button", { name: "Team Chat Room" });
    await expect(
      teamChatBtn.locator('[data-testid="room-unread-badge"]')
    ).toBeVisible({ timeout: 5_000 });

    await setTabHidden(page);

    await expect(page).toHaveTitle(/^\(\d+\) River - Public Discussion Room$/, {
      timeout: 2_000,
    });
  });

  // Same edge case with NO room ever selected: there is no "current room"
  // to protect, so pre-existing unread across the whole example fixture
  // surfaces on the very first hide.
  test("hiding the tab with no room selected surfaces existing unread", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // No room is auto-selected, so the visible-tab title starts plain.
    await expect(page).toHaveTitle("River", { timeout: 5_000 });

    await setTabHidden(page);

    await expect(page).toHaveTitle(/^\(\d+\) River$/, { timeout: 2_000 });
  });

  // The other side of the same coin: once every room with unread messages
  // has actually been opened (and thereby marked read), hiding the tab must
  // NOT invent a badge out of nothing.
  test("hiding the tab after reading every room keeps the plain title", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await openRoom(page, "Public Discussion Room");
    await openRoom(page, "Team Chat Room");
    await expect(page).toHaveTitle("River - Team Chat Room", {
      timeout: 5_000,
    });

    await setTabHidden(page);

    await expect(page).not.toHaveTitle(/^\(\d+\) /, { timeout: 2_000 });
    await expect(page).toHaveTitle("River - Team Chat Room");
  });

  // Defensive: once nothing is unread, a hide -> show -> hide cycle must
  // not introduce a badge on the second hide either (e.g. by counting
  // messages twice or toggling state in the wrong direction).
  test("hide -> show -> hide cycle stays badge-free once everything is read", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await openRoom(page, "Public Discussion Room");
    await openRoom(page, "Team Chat Room");
    await expect(page).toHaveTitle("River - Team Chat Room", {
      timeout: 5_000,
    });

    await setTabHidden(page);
    await expect(page).toHaveTitle("River - Team Chat Room");

    await setTabVisible(page);
    await expect(page).toHaveTitle("River - Team Chat Room");

    await setTabHidden(page);
    await expect(page).not.toHaveTitle(/^\(\d+\) /, { timeout: 2_000 });
    await expect(page).toHaveTitle("River - Team Chat Room");
  });
});
