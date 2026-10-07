import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectListedRoom, setTabVisibility } from "./example-room";
import {
  AT_BOTTOM_EPSILON_PX,
  BOTTOM_THRESHOLD_PX,
  deliverOffscreen,
  distanceFromBottom,
  expectParkedAwayFromEnd,
  expectReadingRow,
  expectRowHeld,
  expectStaysPut,
  fillHistory,
  newestRowFromViewBottom,
  nextFrames,
  openRoomAtBottom,
  readerReturnsToEnd,
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
// is the Latest button offered, and is the room marked read? The 10c policy:
// Latest shows whenever the newest message's bottom is off screen, without the
// old 100px band (decision 4), and the open room counts as read only while the
// tab and chat panel are visible and its newest message is on screen
// (decision 5). The viewport witnesses (`onScreen`, `belowView`) are
// independent of both.
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

  /// How far the text's bubble reaches below the visible part of the history.
  function belowView(page: Page, text: string): Promise<number> {
    return page.getByText(text, { exact: false }).evaluate((el) => {
      const c = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      return el.getBoundingClientRect().bottom - c.bottom;
    });
  }

  const latest = (page: Page) => page.getByTestId("scroll-to-bottom");

  /// Team Chat's badge in the room list. Hidden while it is the current room,
  /// so callers open another room first; the room-list click marks only the
  /// room it opens.
  const teamChatBadge = (page: Page) =>
    page
      .getByTestId("room-list")
      .getByRole("button", { name: "Team Chat Room" })
      .locator('[data-testid="room-unread-badge"]');

  /// Open Team Chat with history to scroll back through, read to its end, and
  /// park at its top.
  async function parkInTeamChat(page: Page) {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page);
  }

  test("an arrival below a parked reader stays off screen, offers Latest, and leaves the room unread", async ({
    page,
  }) => {
    await parkInTeamChat(page);
    const before = await scrollTop(page);

    await deliverOffscreen(page, "short unseen arrival");
    expect(await onScreen(page, "short unseen arrival"), "the arrival was brought on screen").toBe(false);
    expect(await scrollTop(page), "the arrival moved a parked reader").toBeCloseTo(before, 0);
    await expect(latest(page), "no Latest offered for an unseen arrival").toBeVisible();

    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "an arrival that never reached the viewport was marked read").toBeVisible();
  });

  test("reaching the newest message marks the room read", async ({ page }) => {
    await parkInTeamChat(page);
    await deliverOffscreen(page, "arrival the reader scrolls down to");

    await readerReturnsToEnd(page);
    await expect
      .poll(() => onScreen(page, "arrival the reader scrolls down to"), {
        message: "premise: the reader reached the arrival",
      })
      .toBe(true);

    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "the reader reached the newest message and the room still counts it unread").toHaveCount(0);
  });

  test("hiding the tab while scrolled up does not mark the room read", async ({ page }) => {
    await parkInTeamChat(page);
    await deliverOffscreen(page, "arrival before the tab hides");

    await setTabVisibility(page, "hidden");
    await setTabVisibility(page, "visible");
    expect(await onScreen(page, "arrival before the tab hides"), "premise: the reader is still parked above the arrival").toBe(
      false,
    );

    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "hiding the tab marked an unseen arrival read").toBeVisible();
  });

  test("a reader parked inside the old 100px band is offered Latest, and an arrival below them leaves the view alone", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await expectStaysPut(page, "premise: the history was still moving after the fillers");
    // Up, but inside the 100px band that used to count as "at the bottom".
    await readerScrollsWithoutGesture(page, (await scrollTop(page)) - 60);
    const distance = await distanceFromBottom(page);
    expect(distance, "premise: inside the old bottom band").toBeGreaterThan(40);
    expect(distance, "premise: inside the old bottom band").toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);
    await expect(latest(page), "the newest message's bottom is off screen and Latest is not offered").toBeVisible();

    const row = await expectReadingRow(page);
    await deliverOffscreen(page, "short arrival inside the band");
    await expectRowHeld(page, row.key, row.top, "an arrival moved a reader inside the old bottom band");
    expect(await onScreen(page, "short arrival inside the band"), "the arrival was brought on screen").toBe(false);
    await expect(latest(page)).toBeVisible();
  });

  test("a short arrival just below a reader at the end shows Latest", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await expect(latest(page), "premise: no Latest at the newest message").toHaveCount(0);

    const row = await expectReadingRow(page);
    await deliverOffscreen(page, "short arrival below the end");
    await expectRowHeld(page, row.key, row.top, "an arrival moved a reader at the end");
    expect(
      await belowView(page, "short arrival below the end"),
      "the arrival's bottom was brought on screen",
    ).toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
    await expect(latest(page), "the newest message's bottom is off screen and Latest is not offered").toBeVisible();
  });

  /// conversation.rs `NEWEST_IN_VIEW_SLACK_PX`: how far below the visible
  /// history the newest message's bottom may sit and still count as on screen.
  const NEWEST_IN_VIEW_SLACK_PX = 4;

  /// Scroll so the newest row's bottom sits `below` px under the visible
  /// history's bottom edge (negative: above it). Returns where it landed.
  async function parkNewestBottom(page: Page, below: number): Promise<number> {
    const target = await page.evaluate((b) => {
      const c = document.getElementById("chat-scroll-container")!;
      const rows = c.querySelectorAll("[data-item-key]");
      const newestBottom = rows[rows.length - 1].getBoundingClientRect().bottom;
      return c.scrollTop + newestBottom - c.getBoundingClientRect().bottom - b;
    }, below);
    await readerScrollsWithoutGesture(page, target);
    return -(await newestRowFromViewBottom(page)).gap;
  }

  /// The history's own padding must not count as part of the newest message:
  /// the slack is a few px past the message's bottom, not past the padding
  /// below it (10c decisions 4 and 5).
  test("the newest message's bottom just below the view offers Latest and leaves the room unread", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await deliverOffscreen(page, "newest near the bottom edge");

    const below = await parkNewestBottom(page, 10);
    expect(below, "premise: the newest message's bottom is past the slack").toBeGreaterThan(NEWEST_IN_VIEW_SLACK_PX + 2);
    expect(below, "premise: ...by a few px").toBeLessThan(14);

    await expect(latest(page), "the newest message's bottom is below the view and Latest is not offered").toBeVisible();
    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "a message whose bottom never reached the view was marked read").toBeVisible();
  });

  test("the newest message's bottom just inside the view hides Latest and marks the room read", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await deliverOffscreen(page, "newest near the bottom edge");

    const below = await parkNewestBottom(page, -6);
    expect(below, "premise: the newest message's bottom is inside the view").toBeLessThan(-2);
    expect(below, "premise: ...by a few px, closer than the history's bottom padding").toBeGreaterThan(-11);

    await expect(latest(page), "all of the newest message is on screen and Latest is still offered").toHaveCount(0);
    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "all of the newest message was on screen and the room still counts it unread").toHaveCount(0);
  });

  // A history short enough to show an arrival in full, so it lands on screen
  // with the tab hidden. On screen is not enough: the tab has to be visible too.
  test.describe("in a hidden tab", () => {
    test.use({ viewport: { width: 1280, height: 2400 } });

    /// The (N) of a hidden tab's title: unread across every room, this one included.
    async function hiddenTitleCount(page: Page): Promise<number> {
      const counted = /^\((\d+)\) /.exec(await page.title());
      return counted ? Number(counted[1]) : 0;
    }

    test("an arrival on screen stays unread until the tab is visible again", async ({ page }) => {
      await openRoomAtBottom(page, "Team Chat Room");
      await setTabVisibility(page, "hidden");
      await expect(page, "premise: the hidden title counts the other rooms").toHaveTitle(
        /^\(\d+\) River - Team Chat Room$/,
      );
      const before = await hiddenTitleCount(page);

      await deliverOffscreen(page, "arrival in a hidden tab");
      expect(await onScreen(page, "arrival in a hidden tab"), "premise: the arrival is on screen").toBe(true);
      expect(await belowView(page, "arrival in a hidden tab"), "premise: all of the arrival is on screen").toBeLessThanOrEqual(0);
      await expect(page, "an arrival seen only in a hidden tab was marked read").toHaveTitle(
        `(${before + 1}) River - Team Chat Room`,
      );
      // `toHaveTitle` passes on its first matching poll, which can precede a mark.
      await nextFrames(page);
      await page.waitForTimeout(300);
      expect(await hiddenTitleCount(page), "an arrival seen only in a hidden tab was marked read").toBe(before + 1);

      await setTabVisibility(page, "visible");
      await nextFrames(page);
      await selectListedRoom(page, "Public Discussion Room");
      await expect(
        teamChatBadge(page),
        "the tab came back with the arrival on screen and the room still counts it unread",
      ).toHaveCount(0);
    });
  });
});

// A room whose chat panel is hidden behind the mobile Rooms or Members panel
// has no layout, so nothing in it is on screen and an arrival there stays
// unread (10c decision 5). Observed after opening another room, through the
// hamburger badge, which counts every room but the current one.
test.describe("Unread behind the mobile panels (A07)", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  const hamburgerBadge = (page: Page) =>
    page
      .getByTestId("hamburger-rooms-button")
      .filter({ visible: true })
      .getByTestId("hamburger-unread-badge");

  test("an arrival while the chat is behind the room list leaves its room unread", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
    await expect(page.getByTestId("room-list")).toBeVisible();
    await expect(page.locator("#chat-scroll-container"), "premise: the chat panel is hidden").toBeHidden();

    await deliverOffscreen(page, "arrived behind the room list");
    await selectListedRoom(page, "Public Discussion Room");

    await expect(hamburgerBadge(page), "an arrival behind the room list was marked read").toHaveText("1");
  });

  test("an arrival while the chat is behind the members panel leaves its room unread", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await page.getByTestId("header-members-button").click();
    const members = page.locator("aside").filter({ hasText: "Active Members" });
    await expect(members).toBeVisible();
    await expect(page.locator("#chat-scroll-container"), "premise: the chat panel is hidden").toBeHidden();

    await deliverOffscreen(page, "arrived behind the members panel");
    // As a notification click does, with the members panel still in front.
    await callRiverTest(page, "switchRoom", "Public Discussion Room");
    await members.locator("button").first().click();
    await expect(page.getByRole("heading", { name: "Public Discussion Room" })).toBeVisible();
    await nextFrames(page);

    await expect(hamburgerBadge(page), "an arrival behind the members panel was marked read").toHaveText("1");
  });
});
