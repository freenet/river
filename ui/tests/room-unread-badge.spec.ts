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

  // freenet/river#500: a room set to "Muted" must not badge, even though it
  // has unread messages, and must not inflate the title / hamburger totals.
  // The example build seeds "Your Private Room" as Muted (example_data.rs) so
  // this is reachable from the browser at all — the bell modal only opens
  // from the CURRENT room's header, and opening a room marks it read.
  test("a muted room shows no badge despite having unread messages", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const muted = page.getByRole("button", { name: "Your Private Room" });
    await expect(muted).toBeVisible({ timeout: 5_000 });

    // The two non-muted rooms badge as usual…
    for (const name of ["Public Discussion Room", "Team Chat Room"]) {
      await expect(
        page.getByRole("button", { name }).locator('[data-testid="room-unread-badge"]')
      ).toBeVisible({ timeout: 5_000 });
    }

    // …while the muted one never does.
    await expect(
      muted.locator('[data-testid="room-unread-badge"]')
    ).toHaveCount(0);
  });
});

test.describe("Muted rooms and the cross-surface totals", () => {
  // The hamburger badge is `md:hidden`, so this needs a mobile viewport.
  test.use({ viewport: { width: 390, height: 844 } });

  // freenet/river#500: the room-row badges, the document title and the
  // hamburger badge must all sum the SAME per-room values. Example data has
  // no direct messages, so the hamburger total is exactly the sum of the
  // room badges — and a muted room contributes to neither.
  test("hamburger total equals the sum of the room badges", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const hamburgerBadge = page.locator(
      '[data-testid="hamburger-rooms-button"] [data-testid="hamburger-unread-badge"]'
    );
    await expect(hamburgerBadge).toBeVisible({ timeout: 5_000 });
    const total = Number(await hamburgerBadge.textContent());
    expect(total).toBeGreaterThan(0);

    // Open the rooms panel and add up every rendered room badge.
    await page.locator('[data-testid="hamburger-rooms-button"]').click();
    await expect(page.locator('[data-testid="room-list"]')).toBeVisible({
      timeout: 5_000,
    });
    const badges = await page
      .locator('[data-testid="room-list"] [data-testid="room-unread-badge"]')
      .allTextContents();

    // The muted room renders no badge, so only two of the three rooms do.
    expect(badges).toHaveLength(2);
    const sum = badges.reduce((acc, t) => acc + Number(t), 0);
    expect(sum).toBe(total);
  });
});
