import { test, expect, Page } from "@playwright/test";

// Regression test for: unread message counts were surfaced in the document
// <title> and the DM rail, but rooms in the Rooms list had no unread
// indicator. Users who don't receive browser notifications (e.g. not
// connected to a localhost node) had no way to tell which rooms had new
// messages.
//
// Requested by Ian Clarke, 2026-05-20.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

test.describe("Rooms list unread badge", () => {
  // Force a desktop viewport so the room rail is visible on the mobile
  // Playwright projects too.
  test.use({ viewport: { width: 1280, height: 800 } });

  test("a room with unread messages shows a numeric badge", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // "Public Discussion Room" — the local user is only an observer, so
    // every example message is authored by someone else and (with no
    // last-read marker yet) counts as unread.
    const roomBtn = page.getByRole("button", {
      name: "Public Discussion Room",
    });
    await expect(roomBtn).toBeVisible({ timeout: 5_000 });

    const badge = roomBtn.locator('[data-testid="room-unread-badge"]');
    await expect(badge).toBeVisible();
    await expect(badge).toHaveText(/^\d+$/);
    expect(Number(await badge.textContent())).toBeGreaterThan(0);
  });

  test("selecting a room clears its unread badge", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const roomBtn = page.getByRole("button", {
      name: "Public Discussion Room",
    });
    await expect(roomBtn).toBeVisible({ timeout: 5_000 });
    await expect(roomBtn.locator('[data-testid="room-unread-badge"]')).toBeVisible();

    await roomBtn.click();
    await expect(
      page.getByRole("heading", { name: "Public Discussion Room" })
    ).toBeVisible({ timeout: 5_000 });

    // Opening the room marks every message read, so the badge disappears.
    await expect(roomBtn.locator('[data-testid="room-unread-badge"]')).toHaveCount(0, {
      timeout: 5_000,
    });
  });
});
