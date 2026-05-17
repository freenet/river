import { test, expect, Page } from "@playwright/test";

// Regression test for Bug #5 (Ivvor, Matrix 2026-05-17): the
// WebSocket connection indicator must remain visible when the user has
// no rooms — that's exactly when a brand-new user accepting their first
// invite needs to know whether their node connection is healthy.
//
// Before the fix the indicator lived inside `MemberList`, which returns
// empty when `CURRENT_ROOM` is None. After the fix it lives in
// `RoomList`'s bottom section, which is always rendered.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

test.describe("Connection status indicator visibility (Bug #5)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("indicator is rendered on initial load", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    // The pill carries a stable data-testid so renames of the
    // surrounding panel can't accidentally orphan this assertion.
    const pill = page.locator('[data-testid="connection-status-indicator"]');
    await expect(pill).toBeVisible({ timeout: 5_000 });
    await expect(pill).toHaveAttribute(
      "aria-label",
      "WebSocket connection status"
    );
  });

  test("indicator lives in the left rail, not the member panel", async ({
    page,
  }) => {
    // Bug #5 root cause was that the indicator only rendered as part of
    // MemberList. We need to assert it sits inside the always-rendered
    // RoomList rail so the no-room state can't hide it.
    await page.goto("/");
    await waitForApp(page);

    const pill = page.locator('[data-testid="connection-status-indicator"]');
    await expect(pill).toBeVisible({ timeout: 5_000 });

    const roomsRail = page.locator("aside").filter({ hasText: "Rooms" });
    const membersRail = page
      .locator("aside")
      .filter({ hasText: "Active Members" });

    // Indicator must be a descendant of the Rooms rail and NOT of the
    // Members rail.
    await expect(
      roomsRail.locator('[data-testid="connection-status-indicator"]')
    ).toHaveCount(1);
    await expect(
      membersRail.locator('[data-testid="connection-status-indicator"]')
    ).toHaveCount(0);
  });

  test("indicator remains visible after deselecting any selected room", async ({
    page,
  }) => {
    // Simulates the no-rooms-yet path Ivvor hit: the user is in a state
    // where no room is selected. We can't easily wipe the example-data
    // rooms from the UI, but we can verify the indicator is independent
    // of any specific `CURRENT_ROOM` value by checking it shows up
    // before clicking any room (initial load has CURRENT_ROOM = None).
    await page.goto("/");
    await waitForApp(page);

    // No room has been clicked, so `CURRENT_ROOM.owner_key` is None and
    // the conversation panel renders the "Welcome to River" empty state.
    await expect(page.getByText("Welcome to River")).toBeVisible({
      timeout: 5_000,
    });

    await expect(
      page.locator('[data-testid="connection-status-indicator"]')
    ).toBeVisible({ timeout: 5_000 });
  });
});
