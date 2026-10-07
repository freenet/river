import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectListedRoom } from "./example-room";
import {
  BOTTOM_THRESHOLD_PX,
  deliverOffscreen,
  distanceFromBottom,
  expectStaysPut,
  fillHistory,
  openRoomAtBottom,
  readerScrollsWithoutGesture,
  scrollTop,
} from "./history-geometry";

// Regression test for: unread message counts were surfaced in the document
// <title> and the DM rail, but rooms in the Rooms list had no unread
// indicator. Users who don't receive browser notifications (e.g. not
// connected to a localhost node) had no way to tell which rooms had new
// messages.
//
// Requested by Ian Clarke, 2026-05-20.

test.describe("Rooms list unread badge", { tag: "@chromium-only" }, () => {
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

test.describe("Muted rooms and the cross-surface totals", { tag: "@chromium-only" }, () => {
  // The hamburger badge is `md:hidden`, so this needs a mobile viewport.
  test.use({ viewport: { width: 390, height: 844 } });

  // freenet/river#500: the room-row badges, the document title and the
  // hamburger badge must all sum the SAME per-room values. Example data has
  // no direct messages, so the hamburger total is exactly the sum of the
  // room badges — and a muted room contributes to neither.
  //
  // That "no DMs" premise is load-bearing and NOT general: `panel_unread`
  // includes DM unread, which has no room badge. If the fixture ever seeds a
  // DM this fails with a diff that reads like a counting regression, so read
  // this comment before chasing it.
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
    // The muted room renders no badge, so only two of the three rooms do.
    // `toHaveCount` retries; `allTextContents()` does not, so assert the count
    // first rather than sampling a half-rendered list into a length check that
    // `retries: 2` would then hide.
    const badgeLocator = page.locator(
      '[data-testid="room-list"] [data-testid="room-unread-badge"]'
    );
    await expect(badgeLocator).toHaveCount(2, { timeout: 5_000 });
    const badges = await badgeLocator.allTextContents();
    const sum = badges.reduce((acc, t) => acc + Number(t), 0);
    expect(sum).toBe(total);
  });
});

// A short arrival related to where the view actually is (A07): is it on screen,
// is the scroll-to-latest button offered, and is the room marked read anyway?
// CURRENT POLICY throughout: 10b changes catch-up and read acknowledgment on
// purpose. The viewport witness is what stays.
test.describe("Unseen arrivals versus the viewport (A07)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  /// Whether the text's bubble overlaps the visible part of the history.
  function onScreen(page: Page, text: string): Promise<boolean> {
    return page.getByText(text, { exact: false }).evaluate((el) => {
      const r = el.getBoundingClientRect();
      const c = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      return r.bottom > c.top && r.top < c.bottom;
    });
  }

  test("an arrival below a parked reader stays off screen, offers catch-up, and still marks the room read", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    expect(await distanceFromBottom(page), "premise: parked away from the end").toBeGreaterThan(
      BOTTOM_THRESHOLD_PX,
    );
    const before = await scrollTop(page);

    await deliverOffscreen(page, "short unseen arrival");
    expect(await onScreen(page, "short unseen arrival"), "the arrival was brought on screen").toBe(false);
    expect(await scrollTop(page), "the arrival moved a parked reader").toBeCloseTo(before, 0);
    await expect(page.getByTestId("scroll-to-bottom"), "no catch-up offered for an unseen arrival").toBeVisible();

    // CURRENT POLICY: the open, visible room is marked read up to its newest
    // message although that message never reached the viewport.
    await selectListedRoom(page, "Public Discussion Room");
    const teamChat = page.getByTestId("room-list").getByRole("button", { name: "Team Chat Room" });
    await expect(teamChat.locator('[data-testid="room-unread-badge"]')).toHaveCount(0);
  });

  test("an arrival for a reader inside the bottom band is followed while catch-up stays hidden", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    // Inside the band the reader still counts as following, so a late layout
    // change from the fillers would snap them back; let it land first.
    await expectStaysPut(page, "the history was still moving after the fillers");
    // Up, but inside the 100px band that still counts as "at the bottom".
    await readerScrollsWithoutGesture(page, (await scrollTop(page)) - 60);
    const distance = await distanceFromBottom(page);
    expect(distance, "premise: inside the bottom band").toBeGreaterThan(40);
    expect(distance, "premise: inside the bottom band").toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);
    await expect(page.getByTestId("scroll-to-bottom"), "premise: no catch-up inside the band").toHaveCount(0);

    await deliverOffscreen(page, "short arrival inside the band");
    // CURRENT POLICY: followed, so hiding catch-up is consistent with the view.
    await expect.poll(() => onScreen(page, "short arrival inside the band")).toBe(true);
    await expect(page.getByTestId("scroll-to-bottom")).toHaveCount(0);
  });
});
