import { test, expect, Page } from "@playwright/test";

// Coverage for: on mobile, a room fills the whole screen, so new messages
// arriving in OTHER rooms were invisible until the user happened to open
// the room list. The top-left hamburger button now carries a numeric badge
// counting unread messages in rooms other than the current one (plus DMs —
// the DM rail lives behind the same button).
//
// Requested by The Torist, 2026-07-22.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

const HAMBURGER = '[data-testid="hamburger-rooms-button"]';
const BADGE = '[data-testid="hamburger-unread-badge"]';

// The example-data build seeds three rooms whose messages are authored by
// other members, so every room starts with unread messages and no
// last-read marker.
const ALL_ROOMS = [
  "Public Discussion Room",
  "Team Chat Room",
  "Your Private Room",
];

test.describe("Mobile hamburger unread badge", () => {
  // Force a mobile viewport so the hamburger (md:hidden) is rendered on
  // the desktop Playwright projects too.
  test.use({ viewport: { width: 390, height: 844 } });

  test("welcome screen hamburger shows total unread across rooms", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // No room selected yet → the welcome screen's hamburger. Every
    // example room has unread messages, so the badge must show.
    const badge = page.locator(`${HAMBURGER} ${BADGE}`);
    await expect(badge).toBeVisible({ timeout: 5_000 });
    await expect(badge).toHaveText(/^\d+$/);
    expect(Number(await badge.textContent())).toBeGreaterThan(0);
  });

  test("badge counts only OTHER rooms and clears once all are read", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const hamburger = page.locator(HAMBURGER);
    const badge = page.locator(`${HAMBURGER} ${BADGE}`);
    await expect(badge).toBeVisible({ timeout: 5_000 });
    const initialTotal = Number(await badge.textContent());

    // Open the first room. Its messages are marked read on open and the
    // current room is excluded from the count, so the badge total must
    // drop but stay non-zero (the other two rooms are still unread).
    await hamburger.click();
    await page.getByRole("button", { name: ALL_ROOMS[0] }).click();
    await expect(
      page.getByRole("heading", { name: ALL_ROOMS[0] })
    ).toBeVisible({ timeout: 5_000 });

    // The read-marker write is deferred (setTimeout 0), so wait for the
    // badge to leave its initial value rather than sampling immediately.
    await expect(badge).not.toHaveText(String(initialTotal), {
      timeout: 5_000,
    });
    const afterFirst = Number(await badge.textContent());
    expect(afterFirst).toBeGreaterThan(0);
    expect(afterFirst).toBeLessThan(initialTotal);

    // Visit the remaining rooms; each visit marks that room read. After
    // the last one every room is read → no badge at all.
    for (const room of ALL_ROOMS.slice(1)) {
      await hamburger.click();
      await page.getByRole("button", { name: room }).click();
      await expect(page.getByRole("heading", { name: room })).toBeVisible({
        timeout: 5_000,
      });
    }

    await expect(badge).toHaveCount(0, { timeout: 5_000 });
  });
});
