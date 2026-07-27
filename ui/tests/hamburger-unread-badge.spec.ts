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
//
// "Your Private Room" is seeded MUTED (freenet/river#500), so it never
// contributes to this badge. It is therefore listed FIRST: with it last, the
// badge would already have reached 0 after the second room and the final
// `toHaveCount(0)` would be satisfied by muting rather than by reading — the
// assertion would pass without ever proving that opening the last room
// cleared anything.
const ALL_ROOMS = [
  "Your Private Room",
  "Public Discussion Room",
  "Team Chat Room",
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

    // No room selected yet → the welcome screen's hamburger. Two of the three
    // example rooms are unread AND unmuted, so the badge must show.
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

    // ALL_ROOMS[0] is the MUTED room, which contributes nothing to this badge,
    // so opening it must leave the total exactly where it was. That is a
    // stronger check than "the total drops" — it pins that muting removes the
    // room from the total rather than merely reducing it.
    await hamburger.click();
    await page.getByRole("button", { name: ALL_ROOMS[0] }).click();
    await expect(page.getByRole("heading", { name: ALL_ROOMS[0] })).toBeVisible({
      timeout: 5_000,
    });
    await expect(badge).toHaveText(String(initialTotal), { timeout: 5_000 });
    // Re-assert after another render pass (opening the panel). `toHaveText`
    // passes on the FIRST matching poll, and the pre-update value matches, so
    // a single check could pass against a stale frame — which would make this
    // the one assertion here that can silently prove nothing.
    await hamburger.click();
    await expect(page.locator('[data-testid="room-list"]')).toBeVisible({
      timeout: 5_000,
    });
    await expect(badge).toHaveText(String(initialTotal));

    // Now a room that DOES count. The read-marker write is deferred
    // (setTimeout 0), so wait for the badge to leave its value rather than
    // sampling immediately. The panel is already open from the re-check above.
    await page.getByRole("button", { name: ALL_ROOMS[1] }).click();
    await expect(page.getByRole("heading", { name: ALL_ROOMS[1] })).toBeVisible({
      timeout: 5_000,
    });
    await expect(badge).not.toHaveText(String(initialTotal), {
      timeout: 5_000,
    });
    const afterFirst = Number(await badge.textContent());
    expect(afterFirst).toBeGreaterThan(0);
    expect(afterFirst).toBeLessThan(initialTotal);

    // Visit the remaining room; that visit marks it read. The last room in the
    // list is an unmuted one, so reaching 0 requires actually reading it.
    for (const room of ALL_ROOMS.slice(2)) {
      await hamburger.click();
      await page.getByRole("button", { name: room }).click();
      await expect(page.getByRole("heading", { name: room })).toBeVisible({
        timeout: 5_000,
      });
    }

    await expect(badge).toHaveCount(0, { timeout: 5_000 });
  });
});
