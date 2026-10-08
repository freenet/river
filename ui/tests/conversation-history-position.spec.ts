import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { selectListedRoom } from "./example-room";
import {
  ALL_PROJECTS,
  WELL_AWAY_FROM_END_PX,
  deliverOffscreen,
  distanceFromBottom,
  expectParkedAwayFromEnd,
  expectReadingRow,
  expectRowHeld,
  expectSettledAtBottom,
  fillHistory,
  hideChatBehindMembers,
  historyHeight,
  holdTestImage,
  knownFailure,
  nextFrames,
  openRoomAtBottom,
  READING_ROW_BUDGET_PX,
  readerScrollsWithoutGesture,
  readingRow,
  rowTop,
  scrollTop,
  viewportHeight,
  withheld,
} from "./history-geometry";

// A parked reader's place when the history changes above them (A02) or while
// the chat panel is hidden or the room empty (A03). The reading row is measured relative to the
// container, by message identity, against READING_ROW_BUDGET_PX.

/// Scroll a settled reader so `key`'s row sits 40px below the top of the
/// visible history.
async function parkWithRowAtTop(page: Page, key: string) {
  const gap = 40;
  const target = await page.evaluate(
    ([k, g]) => {
      const c = document.getElementById("chat-scroll-container")!;
      const row = c.querySelector(`[data-item-key="${CSS.escape(k as string)}"]`)!;
      return c.scrollTop + row.getBoundingClientRect().top - c.getBoundingClientRect().top - (g as number);
    },
    [key, gap] as const,
  );
  await readerScrollsWithoutGesture(page, target);
}

/// The `data-item-key` of the row whose text contains `text`.
async function keyOf(page: Page, text: string): Promise<string> {
  const row = page.locator("[data-item-key]", { hasText: text });
  await expect(row).toHaveCount(1);
  return (await row.getAttribute("data-item-key"))!;
}

async function backToChat(page: Page) {
  await page.locator("aside").filter({ hasText: "Active Members" }).locator("button").first().click();
  await expect(page.locator("#chat-scroll-container")).toBeVisible();
  await nextFrames(page);
}

test.describe("Reading position when content above changes (A02)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // freenet/river#507. Browser scroll anchoring is off on the history, and the
  // app compensates only for window-head swaps, so a Markdown image that loads
  // above a parked reader pushes their text down by its height.
  test("an image loading above a parked reader keeps their row in place", async ({ page }) => {
    const heldImage = await holdTestImage(page);
    await openRoomAtBottom(page, "Team Chat Room");
    // The Markdown renders to text plus an <img>, so wait on the plain words.
    await callRiverTest(page, "appendMessage", "image fixture ![fixture](/test-image.svg) above the reader");
    await expect(page.getByText("image fixture", { exact: false })).toBeVisible({ timeout: 5_000 });
    await fillHistory(page);
    await expect.poll(heldImage.requested, { message: "premise: the image is requested and held" }).toBeGreaterThan(0);

    const imageKey = await keyOf(page, "image fixture");
    const afterImage = await page.evaluate((k) => {
      const c = document.getElementById("chat-scroll-container")!;
      const row = c.querySelector(`[data-item-key="${CSS.escape(k)}"]`)!;
      const rows = Array.from(c.querySelectorAll("[data-item-key]"));
      return rows[rows.indexOf(row) + 1].getAttribute("data-item-key")!;
    }, imageKey);
    await parkWithRowAtTop(page, afterImage);
    await expectParkedAwayFromEnd(page);
    expect(
      (await rowTop(page, imageKey))!,
      "premise: the image row is above the visible history",
    ).toBeLessThan(0);
    const row = await readingRow(page);
    expect(row, "premise: a row is fully in view").not.toBeNull();
    const image = page.locator(`[data-item-key="${imageKey}"] img`);
    const heightBefore = await page.locator(`[data-item-key="${imageKey}"]`).evaluate((r) => r.getBoundingClientRect().height);

    await heldImage.release();
    await expect
      .poll(() => image.evaluate((img: HTMLImageElement) => img.complete && img.naturalHeight > 0), {
        message: "premise: the image should load",
      })
      .toBe(true);
    await expect
      .poll(
        () => page.locator(`[data-item-key="${imageKey}"]`).evaluate((r) => r.getBoundingClientRect().height),
        { message: "premise: the image row should grow when the image lays out" },
      )
      .toBeGreaterThan(heightBefore + 50);

    knownFailure(ALL_PROJECTS, "freenet/river#507");
    await expectRowHeld(page, row!.key, row!.top, "an image loading above the reader moved their row");
  });

  // freenet/river#507: a ban purge or bulk delete removing rows above a parked
  // reader shifts their text up by the removed height.
  test("removing rows above a parked reader keeps their row in place", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, "purge-me");
    await fillHistory(page, "keep");
    await parkWithRowAtTop(page, await keyOf(page, "keep 2:"));
    await expectParkedAwayFromEnd(page);
    const row = await readingRow(page, "keep 2:");
    expect(row, "premise: the reader is looking at a surviving row").not.toBeNull();
    expect(
      (await rowTop(page, await keyOf(page, "purge-me 7:")))!,
      "premise: every removed row is above the visible history",
    ).toBeLessThan(0);

    await callRiverTest(page, "removeMessages", "purge-me");
    await expect(page.getByText("purge-me", { exact: false })).toHaveCount(0, { timeout: 5_000 });

    knownFailure(ALL_PROJECTS, "freenet/river#507");
    await expectRowHeld(page, row!.key, row!.top, "removing rows above the reader moved their row");
  });

  // Deleting the very row the reader is looking at must not be read as "the
  // reader's place is gone, take them to the latest message". An invariant on
  // main and after the scroll simplification.
  test("deleting the row a parked reader is looking at does not navigate to the latest message", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, "keep");
    await fillHistory(page, "tail");
    await parkWithRowAtTop(page, await keyOf(page, "keep 2:"));
    await expectParkedAwayFromEnd(page);
    const row = await readingRow(page, "keep 2:");
    expect(row, "premise: the reader is looking at the row to delete").not.toBeNull();
    // The row above stays put. Not the row below: alternating authors mean
    // the rows either side merge into one group once this one is gone.
    expect(row!.prevKey, "premise: a row precedes it").not.toBeNull();
    const prevTop = (await rowTop(page, row!.prevKey!))!;
    const before = await scrollTop(page);

    await callRiverTest(page, "removeMessages", "keep 2:");
    await expect(page.getByText("keep 2:", { exact: false })).toHaveCount(0, { timeout: 5_000 });
    await nextFrames(page);

    expect(
      await distanceFromBottom(page),
      "deleting the reader's row took them to the latest message",
    ).toBeGreaterThan(WELL_AWAY_FROM_END_PX);
    expect(Math.abs((await scrollTop(page)) - before), "the view moved when the reader's row was deleted").toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
    await expectRowHeld(page, row!.prevKey!, prevTop, "the row above the deleted one moved");
  });
});

test.describe("Deleting the reading row while the chat is hidden (A02)", () => {
  // Below the 768px breakpoint, so the members panel replaces the chat.
  test.use({ viewport: { width: 390, height: 844 } });

  // The visible variant above holds the row above because nothing corrects a
  // mid-window removal. Hidden, the reveal restores the reader from the anchor
  // saved before the panel hid, and that restore has to hold the same row: the
  // one above the deleted row, not the one below (10c decision 7).
  test("deleting the row a reader is looking at while the chat is hidden holds the row above on reveal", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, "keep");
    await fillHistory(page, "tail");
    await parkWithRowAtTop(page, await keyOf(page, "keep 2:"));
    await expectParkedAwayFromEnd(page);
    const row = await readingRow(page, "keep 2:");
    expect(row, "premise: the reader is looking at the row to delete").not.toBeNull();
    expect((await readingRow(page))!.key, "premise: the row to delete is the first one fully in view").toBe(row!.key);
    // Not the row below: alternating authors mean the rows either side merge
    // into one group once this one is gone.
    expect(row!.prevKey, "premise: a row precedes it").not.toBeNull();
    const prevTop = (await rowTop(page, row!.prevKey!))!;

    await hideChatBehindMembers(page);
    await callRiverTest(page, "removeMessages", "keep 2:");
    await expect(page.getByText("keep 2:", { exact: false })).toHaveCount(0, { timeout: 5_000 });
    await backToChat(page);

    await expectParkedAwayFromEnd(page, "the reveal took the reader to the latest message");
    await expectRowHeld(page, row!.prevKey!, prevTop, "the row above the deleted one moved across the hidden deletion");
  });
});

test.describe("Reading position across a hidden chat panel (A03)", () => {
  // Below the 768px breakpoint, so the members panel replaces the chat.
  test.use({ viewport: { width: 390, height: 844 } });

  /// Park 400px above the end of the history and return the row in view.
  async function parkAboveEnd(page: Page) {
    const end = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      return c.scrollHeight - c.clientHeight;
    });
    await readerScrollsWithoutGesture(page, Math.max(0, end - 400));
    const row = await readingRow(page);
    expect(row, "premise: a row is fully in view").not.toBeNull();
    return row!;
  }

  /// Park 400px above the end of the at-cap room, below the rows a drain removes.
  async function parkInCappedRoom(page: Page) {
    await openRoomAtBottom(page, "Capped History Room", "/?deep-history-room=1");
    return parkAboveEnd(page);
  }

  /// Wait for a hidden burst to patch the history.
  async function expectRowCountAbove(page: Page, rows: number, why: string) {
    await expect.poll(() => page.locator("[data-item-key]").count(), { message: why }).toBeGreaterThan(rows);
  }

  // #732's hidden-column findings: the head-swap compensation measures rows
  // with `offsetTop`, which is 0 for every row of a `display:none` panel, so a
  // drain that lands while the chat is hidden is never compensated. The same
  // drain is compensated when visible ("a batched at-cap drain does not crawl
  // a parked reader" in conversation-autoscroll.spec.ts).
  // Arrivals delivered while hidden: enough to overflow the at-cap room's
  // cap and drain its oldest messages from the head.
  const HIDDEN_DRAIN_BATCH = 61;

  // Delivered as two bursts, so the reveal also survives more than one hidden
  // render.
  test("an at-cap drain while the chat is hidden keeps the reader's row", async ({ page }) => {
    const row = await parkInCappedRoom(page);
    const rowsBefore = await page.locator("[data-item-key]").count();

    await hideChatBehindMembers(page);
    await callRiverTest(page, "appendMessages", 30);
    await expectRowCountAbove(page, rowsBefore + 10, "premise: the first burst should patch the hidden history");
    const rowsAfterFirst = await page.locator("[data-item-key]").count();
    await callRiverTest(page, "appendMessages", HIDDEN_DRAIN_BATCH - 30);
    await expectRowCountAbove(page, rowsAfterFirst, "premise: the second burst should patch the hidden history");
    await backToChat(page);

    await expectRowHeld(page, row.key, row.top, "an at-cap drain while the chat was hidden moved the reader's row");
  });

  // No click precedes a breakpoint hide, so the last settle's position is used.
  test("a reader hidden and revealed by the breakpoint keeps their row", async ({ page }) => {
    await openRoomAtBottom(page, "Capped History Room", "/?deep-history-room=1");
    // Members selected on a phone, then widened: every panel shows.
    await hideChatBehindMembers(page);
    await page.setViewportSize({ width: 1280, height: 844 });
    await expect(page.locator("#chat-scroll-container")).toBeVisible();
    await nextFrames(page);
    const row = await parkAboveEnd(page);
    const rowsBefore = await page.locator("[data-item-key]").count();

    // Narrowing hides the chat again, since Members is still selected.
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.locator("#chat-scroll-container"), "premise: the breakpoint hid the chat").toBeHidden();
    await callRiverTest(page, "appendMessages", HIDDEN_DRAIN_BATCH);
    await expectRowCountAbove(page, rowsBefore + 30, "premise: the batch should patch the hidden history");
    await page.setViewportSize({ width: 1280, height: 844 });
    await expect(page.locator("#chat-scroll-container")).toBeVisible();
    await nextFrames(page);

    await expectRowHeld(page, row.key, row.top, "a drain while the breakpoint hid the chat moved the reader's row");
  });

  // A burst past the ceiling while hidden: the range keeps the reader's row
  // and the reveal restores it.
  test("a burst past the render ceiling while the chat is hidden keeps the reader's row rendered", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1");
    // Below the backfill strip (top 800px), near the head of the window.
    await readerScrollsWithoutGesture(page, 1_000);
    const row = await readingRow(page, "history filler");
    expect(row, "premise: a filler row is fully in view").not.toBeNull();
    // 201 seeded + 200 arrivals against a cap of 300 prunes the oldest 101.
    expect(
      Number(/history filler (\d+)/.exec(row!.text)![1]),
      "premise: the reader's message survives the prune",
    ).toBeGreaterThanOrEqual(101);

    await hideChatBehindMembers(page);
    await callRiverTest(page, "appendMessages", 200);
    await expect
      .poll(() => withheld(page), { message: "premise: the burst should take the hidden range past the ceiling" })
      .toBeGreaterThan(0);
    await backToChat(page);

    await expectRowHeld(page, row!.key, row!.top, "a burst past the ceiling while hidden moved the reader's row");
  });

  // CURRENT POLICY: a room opened while the chat is hidden opens at its
  // newest message, and the geometry on reveal belongs to that room, not the
  // one that was hidden. Restoring a saved reading position instead would be
  // a deliberate change to this test.
  test("a room switched to while the chat is hidden opens at its newest message on reveal", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await hideChatBehindMembers(page);

    await callRiverTest(page, "switchRoom", "Your Private Room");
    await backToChat(page);

    await expect(page.getByRole("heading", { name: "Your Private Room" })).toBeVisible();
    await expect(page.getByText("filler 0:", { exact: false }), "the previous room's rows are still rendered").toHaveCount(0);
    await expectSettledAtBottom(page, "the room switched to while hidden did not open at its newest message");
    await page.waitForTimeout(600);
    await expectSettledAtBottom(page, "the newly opened room did not stay at its newest message");
  });
});

test.describe("Opening a room that is temporarily empty (A03)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // The opening waits for rows. A room opened with no messages lands at its
  // newest message when the first ones arrive, once, and then stays put.
  test("a room opened empty lands at its newest message when its first messages arrive", async ({ page }) => {
    const empty = page.getByText("No messages yet", { exact: false });
    await openRoomAtBottom(page, "Public Discussion Room");
    await callRiverTest(page, "removeMessages", "");
    await expect(empty, "premise: the room should now be empty").toBeVisible();
    await selectListedRoom(page, "Team Chat Room");
    await selectListedRoom(page, "Public Discussion Room");
    await expect(empty, "premise: the room opened empty").toBeVisible();

    await callRiverTest(page, "appendMessages", 30);
    await expect(page.getByText("batched arrival 29", { exact: false })).toHaveCount(1, { timeout: 5_000 });
    expect(
      await historyHeight(page),
      "premise: the first messages should overflow the view",
    ).toBeGreaterThan((await viewportHeight(page)) + WELL_AWAY_FROM_END_PX);
    await expectSettledAtBottom(page, "the room opened empty did not open at its newest message");

    const row = await expectReadingRow(page);
    await deliverOffscreen(page, "arrival after the opening");
    await expectRowHeld(page, row.key, row.top, "an arrival after the opening moved the view");
  });
});
