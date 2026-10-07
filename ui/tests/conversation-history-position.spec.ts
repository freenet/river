import { test, expect, Page, Route } from "@playwright/test";
import { callRiverTest } from "./river-test";
import {
  ALL_PROJECTS,
  BOTTOM_THRESHOLD_PX,
  distanceFromBottom,
  expectParkedAwayFromEnd,
  expectRowHeld,
  expectSettledAtBottom,
  fillHistory,
  knownFailure,
  nextFrames,
  openRoomAtBottom,
  READING_ROW_BUDGET_PX,
  readerScrollsWithoutGesture,
  readingRow,
  rowTop,
  scrollTop,
} from "./history-geometry";

// A parked reader's place when the history changes above them (A02) or while
// the chat panel is hidden (A03). The reading row is measured relative to the
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

test.describe("Reading position when content above changes (A02)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // freenet/river#507. Browser scroll anchoring is off on the history, and the
  // app compensates only for window-head swaps, so a Markdown image that loads
  // above a parked reader pushes their text down by its height.
  test("an image loading above a parked reader keeps their row in place", async ({ page }) => {
    let resolveRequested!: (route: Route) => void;
    const requested = new Promise<Route>((resolve) => {
      resolveRequested = resolve;
    });
    await page.route("**/test-image.svg", (route) => resolveRequested(route));

    await openRoomAtBottom(page, "Team Chat Room");
    // The Markdown renders to text plus an <img>, so wait on the plain words.
    await callRiverTest(page, "appendMessage", "image fixture ![fixture](/test-image.svg) above the reader");
    await expect(page.getByText("image fixture", { exact: false })).toBeVisible({ timeout: 5_000 });
    await fillHistory(page);
    const route = await requested;

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

    await route.fulfill({
      contentType: "image/svg+xml",
      body: '<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300"><rect width="400" height="300" fill="#888"/></svg>',
    });
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
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    expect(Math.abs((await scrollTop(page)) - before), "the view moved when the reader's row was deleted").toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
    await expectRowHeld(page, row!.prevKey!, prevTop, "the row above the deleted one moved");
  });
});

test.describe("Reading position across a hidden chat panel (A03)", () => {
  // Below the 768px breakpoint, so the members panel replaces the chat.
  test.use({ viewport: { width: 390, height: 844 } });

  async function hideChatBehindMembers(page: Page) {
    await page.getByTestId("header-members-button").click();
    await expect(page.locator("aside").filter({ hasText: "Active Members" })).toBeVisible();
    await expect(page.locator("#chat-scroll-container"), "premise: the chat panel is hidden").toBeHidden();
  }

  async function backToChat(page: Page) {
    await page.locator("aside").filter({ hasText: "Active Members" }).locator("button").first().click();
    await expect(page.locator("#chat-scroll-container")).toBeVisible();
    await nextFrames(page);
  }

  // #732's hidden-column findings: the head-swap compensation measures rows
  // with `offsetTop`, which is 0 for every row of a `display:none` panel, so a
  // drain that lands while the chat is hidden is never compensated. The same
  // drain is compensated when visible ("a batched at-cap drain does not crawl
  // a parked reader" in conversation-autoscroll.spec.ts).
  // Arrivals delivered while hidden: enough to overflow the at-cap room's
  // cap and drain its oldest messages from the head.
  const HIDDEN_DRAIN_BATCH = 61;

  test("an at-cap drain while the chat is hidden keeps the reader's row", async ({ page }) => {
    await openRoomAtBottom(page, "Capped History Room", "/?deep-history-room=1");
    const parkedAt = Math.max(
      0,
      (await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        return c.scrollHeight - c.clientHeight;
      })) - 400,
    );
    await readerScrollsWithoutGesture(page, parkedAt);
    const row = await readingRow(page);
    expect(row, "premise: a row is fully in view").not.toBeNull();
    const rowsBefore = await page.locator("[data-item-key]").count();

    await hideChatBehindMembers(page);
    await callRiverTest(page, "appendMessages", HIDDEN_DRAIN_BATCH);
    await expect
      .poll(() => page.locator("[data-item-key]").count(), {
        message: "premise: the batch should patch the hidden history, keeping the surviving rows",
      })
      .toBeGreaterThan(rowsBefore + 30);
    await backToChat(page);

    knownFailure(ALL_PROJECTS, "hidden-panel drain is not compensated (freenet/river#732 hidden-column findings)");
    await expectRowHeld(page, row!.key, row!.top, "an at-cap drain while the chat was hidden moved the reader's row");
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
