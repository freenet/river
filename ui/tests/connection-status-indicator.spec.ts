import { test, expect, Page } from "@playwright/test";

// Regression tests for Bug #5 (Ivvor, Matrix 2026-05-17): the WebSocket
// connection indicator must remain visible when the user has no rooms —
// that's exactly when a brand-new user accepting their first invite
// needs to know whether their node connection is healthy.
//
// Before the fix the indicator lived inside `MemberList`, which returns
// empty when `CURRENT_ROOM` is None. After the fix it lives in
// `RoomList`'s bottom section AND in an inline (`md:hidden`) copy on
// the Conversation panel's Welcome screen, so both desktop and mobile
// no-room flows surface it.
//
// Both placements share the `data-testid="connection-status-indicator"`
// id, so `:visible` is used to pick the one the user actually sees at
// the current viewport.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

const PILL = '[data-testid="connection-status-indicator"]';
const VISIBLE_PILL = `${PILL}:visible`;

test.describe("Connection status indicator on desktop (Bug #5)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("a visible indicator is rendered on initial load", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    // The pill carries a stable data-testid so renames of the
    // surrounding panel can't accidentally orphan this assertion.
    const visiblePill = page.locator(VISIBLE_PILL).first();
    await expect(visiblePill).toBeVisible({ timeout: 5_000 });
    await expect(visiblePill).toHaveAttribute(
      "aria-label",
      "WebSocket connection status"
    );
  });

  test("the visible indicator lives in the left rail, not the member panel", async ({
    page,
  }) => {
    // Bug #5 root cause was that the indicator only rendered as part of
    // MemberList. We need to assert the visible indicator at desktop
    // width sits inside the always-rendered Rooms rail so the no-room
    // state can't hide it.
    await page.goto("/");
    await waitForApp(page);

    await expect(page.locator(VISIBLE_PILL).first()).toBeVisible({
      timeout: 5_000,
    });

    const roomsRail = page.locator("aside").filter({ hasText: "Rooms" });
    const membersRail = page
      .locator("aside")
      .filter({ hasText: "Active Members" });

    // The visible pill at desktop width must be inside the Rooms rail.
    await expect(roomsRail.locator(VISIBLE_PILL)).toHaveCount(1);
    // The Members rail must NOT carry an indicator at all (pre-fix
    // location).
    await expect(membersRail.locator(PILL)).toHaveCount(0);
  });

  test("indicator remains visible when no room is selected (Welcome screen)", async ({
    page,
  }) => {
    // Initial load has `CURRENT_ROOM = None` — the conversation panel
    // renders "Welcome to River". The indicator must still be visible.
    await page.goto("/");
    await waitForApp(page);

    await expect(page.getByText("Welcome to River")).toBeVisible({
      timeout: 5_000,
    });

    await expect(page.locator(VISIBLE_PILL).first()).toBeVisible({
      timeout: 5_000,
    });
  });
});

test.describe("Connection status indicator on mobile (Bug #5)", () => {
  // Mobile-Chat is the default `MOBILE_VIEW`, so a brand-new user with
  // no rooms lands on the Welcome screen with the left rail hidden
  // behind `hidden md:flex`. Without the inline Welcome-screen copy of
  // the indicator, Bug #5 would reappear on mobile.
  test.use({ viewport: { width: 375, height: 812 } });

  test("a visible indicator is shown on the mobile Welcome screen", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    await expect(page.getByText("Welcome to River")).toBeVisible({
      timeout: 5_000,
    });

    // The left-rail copy is hidden at this width, so the visible
    // indicator must be the inline one inside the Conversation panel.
    const visiblePill = page.locator(VISIBLE_PILL).first();
    await expect(visiblePill).toBeVisible({ timeout: 5_000 });

    // Confirm we're looking at the inline (not left-rail) copy.
    const inWelcomeScreen = page
      .locator("h1", { hasText: "Welcome to River" })
      .locator("..")
      .locator(VISIBLE_PILL);
    await expect(inWelcomeScreen).toHaveCount(1);
  });
});
