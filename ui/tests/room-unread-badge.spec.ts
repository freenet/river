import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import {
  hiddenTitleCount,
  roomUnreadBadge,
  selectListedRoom,
  setTabVisibility,
  waitForApp,
} from "./example-room";
import {
  NEWEST_IN_VIEW_SLACK_PX,
  backToChatFromMembers,
  deliverOffscreen,
  expectParkedAwayFromEnd,
  expectReadingRow,
  expectRowHeld,
  expectSettledAtBottom,
  fillHistory,
  hideChatBehindMembers,
  newestRowFromViewBottom,
  nextFrames,
  openRoomAtBottom,
  readerReturnsToEnd,
  readerScrollsWithoutGesture,
  scrollMetrics,
  settle,
} from "./history-geometry";
import { DM_PEER, closeThread, newestHistoryDm, openThread } from "./dm-thread";

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
// is the Latest button offered, and is the room marked read? The rule:
// Latest shows whenever the newest message's bottom is off screen, without the
// old 100px band, and the open room counts as read only while the
// tab and chat panel are visible and its newest message is on screen.
// The viewport witnesses (`onScreen`, `belowView`) are independent of both.
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

  /// Team Chat's badge in the room list; the room-list click marks only the
  /// room it opens.
  const teamChatBadge = (page: Page) => roomUnreadBadge(page, "Team Chat Room");

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
    const before = (await scrollMetrics(page)).top;

    await deliverOffscreen(page, "short unseen arrival");
    expect(await onScreen(page, "short unseen arrival"), "the arrival was brought on screen").toBe(false);
    expect((await scrollMetrics(page)).top, "the arrival moved a parked reader").toBeCloseTo(before, 0);
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
  /// below it.
  test("the newest message's bottom just below the view offers Latest and leaves the room unread", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    // Parked away, so the arrival is not followed (and read) on arrival.
    await readerScrollsWithoutGesture(page, 0);
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
    // Parked away, so the arrival is not followed (and read) on arrival.
    await readerScrollsWithoutGesture(page, 0);
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
      // `toHaveTitle` passes on its first matching poll, which can precede a
      // mark; `hiddenTitleCount` settles first.
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

  // The same short history, read under a DM thread: an arrival on screen
  // behind one is not seen until it closes, and closing it reads without
  // scrolling. Other dialogs do not cover the room.
  test.describe("under a DM thread", { tag: "@chromium-only" }, () => {
    test.use({ viewport: { width: 1280, height: 2400 } });

    async function openDmThread(page: Page) {
      await callRiverTest(page, "appendDmsForPeer", DM_PEER.index, 1);
      await openThread(page, DM_PEER, newestHistoryDm(DM_PEER, 1));
    }

    test("an arrival on screen under a DM thread stays unread", async ({ page }) => {
      await openRoomAtBottom(page, "Team Chat Room");
      await openDmThread(page);

      await deliverOffscreen(page, "arrival under a DM thread");
      expect(await belowView(page, "arrival under a DM thread"), "premise: all of the arrival is on screen").toBeLessThanOrEqual(0);
      await nextFrames(page);
      // Away with the thread still open, so its closing cannot read the room.
      await callRiverTest(page, "switchRoom", "Public Discussion Room");
      await expect(teamChatBadge(page), "an arrival seen only under a DM thread was marked read").toBeVisible();
    });

    test("closing the DM thread reads an arrival on screen without moving the view", async ({ page }) => {
      await openRoomAtBottom(page, "Team Chat Room");
      await openDmThread(page);
      await deliverOffscreen(page, "arrival read on close");
      expect(await belowView(page, "arrival read on close"), "premise: all of the arrival is on screen").toBeLessThanOrEqual(0);
      const before = (await scrollMetrics(page)).top;

      await closeThread(page);
      await nextFrames(page);
      expect((await scrollMetrics(page)).top, "closing the DM thread moved the view").toBeCloseTo(before, 0);
      await selectListedRoom(page, "Public Discussion Room");
      await expect(teamChatBadge(page), "closing the DM thread left an arrival on screen unread").toHaveCount(0);
    });
  });
});

// While the members panel replaces the chat on a phone, Latest is hidden with
// the chat and so is the room list, so the members panel's back arrow counts
// the open room's unread messages under its notification mode: the same count
// the open room's row shows behind the rooms panel. The rooms panel's back
// arrow has no badge, since that row is on screen beside it.
test.describe("Members back arrow counts the open room's unread messages", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  const backArrow = (page: Page) => page.getByTestId("members-back-to-chat-button");
  const membersBadge = (page: Page) => backArrow(page).getByTestId("members-current-room-unread-badge");

  /// Pick the open room's notification mode from the bell in its header.
  async function setNotificationMode(page: Page, option: "Muted" | "Mentions & replies only") {
    await page.getByTestId("notification-bell-button").click();
    const modal = page.getByTestId("notification-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });
    await modal.getByTestId("notification-mode-option").filter({ hasText: option }).click();
    await expect(modal).toHaveCount(0, { timeout: 5_000 });
  }

  /// The local user's member id label, as a mention token references it.
  async function selfMemberId(page: Page): Promise<string> {
    const row = page.getByTestId("member-list").locator('li:has([data-testid="member-list-self"])');
    await expect(row, "premise: the member list shows the local user").toHaveCount(1);
    const testid = await row.getAttribute("data-testid");
    expect(testid, "premise: the local user's row carries its member id").toMatch(/^member-item-/);
    return testid!.slice("member-item-".length);
  }

  test("an arrival while the members panel shows counts on its back arrow", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await hideChatBehindMembers(page);
    await expect(membersBadge(page), "premise: nothing unread yet").toHaveCount(0);

    await deliverOffscreen(page, "first arrival behind the members panel");
    await expect(membersBadge(page), "the back arrow does not count the arrival").toHaveText("1");
    await expect(backArrow(page)).toHaveAccessibleName("Back to chat, 1 unread message");

    await deliverOffscreen(page, "second arrival behind the members panel");
    await expect(membersBadge(page), "the back arrow does not count the second arrival").toHaveText("2");
    await expect(backArrow(page)).toHaveAccessibleName("Back to chat, 2 unread messages");
  });

  test("with nothing unread the back arrow shows no badge", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await hideChatBehindMembers(page);
    await settle(page);

    await expect(membersBadge(page), "a badge with nothing unread").toHaveCount(0);
    await expect(backArrow(page)).toHaveAccessibleName("Back to chat");
  });

  test("a muted room shows no badge on the back arrow", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await setNotificationMode(page, "Muted");
    await hideChatBehindMembers(page);

    await deliverOffscreen(page, "arrival in a muted room");
    await settle(page);
    await expect(membersBadge(page), "a muted room badged its arrival").toHaveCount(0);
    await expect(backArrow(page)).toHaveAccessibleName("Back to chat");
  });

  test("a mentions-and-replies room counts only arrivals that mention the reader", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await setNotificationMode(page, "Mentions & replies only");
    await hideChatBehindMembers(page);
    const me = await selfMemberId(page);

    await deliverOffscreen(page, "arrival that mentions nobody");
    await settle(page);
    await expect(membersBadge(page), "an arrival that mentions nobody was counted").toHaveCount(0);

    // The token renders as a mention chip, so wait on the plain text after it.
    await callRiverTest(page, "appendMessage", `@[Me](rv:${me}) arrival that mentions the reader`);
    await expect(page.getByText("arrival that mentions the reader", { exact: false })).toHaveCount(1, {
      timeout: 5_000,
    });
    await expect(membersBadge(page), "an arrival that mentions the reader was not counted").toHaveText("1");
    await expect(backArrow(page)).toHaveAccessibleName("Back to chat, 1 unread mention or reply");
  });

  // A hidden chat has no layout, so the arrival is not followed: back in the
  // chat it sits below the reader's restored row, Latest offers it, and it
  // stays unread until the reader reaches it.
  test("returning to the chat leaves the counted arrival below and unread until reached", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const row = await expectReadingRow(page);

    await hideChatBehindMembers(page);
    await deliverOffscreen(page, `tall arrival behind members ${"word ".repeat(300)}`);
    await expect(membersBadge(page), "premise: the back arrow counts the arrival").toHaveText("1");
    await backToChatFromMembers(page);

    await expectRowHeld(page, row.key, row.top, "an arrival behind the members panel moved a reader at the end");
    await expect(page.getByTestId("scroll-to-bottom"), "Latest should offer the hidden arrival").toBeVisible();

    // The open room's row behind the rooms panel shows the same count, and
    // that panel's own back arrow has no badge.
    await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
    await expect(roomUnreadBadge(page, "Team Chat Room"), "the open room's row disagrees with the back arrow").toHaveText(
      "1",
    );
    await expect(page.getByTestId("rooms-back-to-chat-button").locator("span[aria-hidden]")).toHaveCount(0);
    await expect(page.getByTestId("rooms-back-to-chat-button")).toHaveAccessibleName("Back to chat");
    await page.getByTestId("rooms-back-to-chat-button").click();
    await expect(page.locator("#chat-scroll-container")).toBeVisible();
    await nextFrames(page);

    // Still unread after two returns that did not reach it.
    await hideChatBehindMembers(page);
    await settle(page);
    await expect(membersBadge(page), "returning to the chat marked an unreached arrival read").toHaveText("1");
    await backToChatFromMembers(page);

    await page.getByTestId("scroll-to-bottom").click();
    await expectSettledAtBottom(page, "Latest did not reach the arrival");
    await settle(page);
    await hideChatBehindMembers(page);
    await expect(membersBadge(page), "reaching the arrival left it unread").toHaveCount(0);
  });
});
