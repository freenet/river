import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom, setTabVisibility } from "./example-room";
import { AT_BOTTOM_EPSILON_PX, nextFrames } from "./history-geometry";

// Where a DM thread's view goes (10c, DM parity): opening it lands on the
// newest DM, an own send jumps to the end once, and Latest jumps there on
// request. Nothing else moves the view. An inbound DM never does, even when
// the reader is at the end; it lands below and Latest offers it.
//
// And when the thread counts as seen (10c decision 11, the rooms' rule): only
// up to an inbound DM that was on screen with the tab visible. One that lands
// below the fold, or arrives while the tab is hidden, stays unread.
//
// The thread is populated through the `appendDms` / `deliverDm` test hooks,
// which add sender-signed DMs from one test member ("DM Test Peer") to self
// and never scroll. The thread is opened from its row in the DM rail.

/// test_hooks.rs `DM_PEER_NICKNAME`.
const PEER = "DM Test Peer";
/// DMs `appendDms` seeds before the thread opens, "dm history 00" first.
const HISTORY = 30;
const NEWEST_HISTORY = `dm history ${HISTORY - 1}`;
const THREAD = "#dm-scroll-container";
const LATEST = "dm-scroll-to-latest";
/// dm_thread_modal.rs `NEWEST_DM_IN_VIEW_SLACK_PX`: how far below the visible
/// area the newest DM's bottom may sit and still count as on screen.
const SLACK_PX = 4;
/// How far a view that must not move may still drift (subpixel rounding).
const STILL_PX = 1;

async function openThread(page: Page) {
  const row = page.locator(".dm-rail-row-btn", { hasText: PEER });
  // Phone widths keep the DM rail in the rooms panel, behind the hamburger.
  const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
  if (!(await row.isVisible()) && (await hamburger.count()) > 0) await hamburger.click();
  await row.click();
  await expect(page.locator(THREAD)).toBeVisible({ timeout: 5_000 });
}

/// A thread too short to scroll ("dm history 00" to "02"), open and settled,
/// so the next inbound DM lands on screen and is the only thing that changes.
async function openShortThread(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  await callRiverTest(page, "appendDms", 3);
  await openThread(page);
  await expect(dm(page, "dm history 02")).toHaveCount(1, { timeout: 5_000 });
  expect((await threadGeometry(page)).max, "premise: the thread does not scroll").toBeLessThanOrEqual(0);
  // The opening's own checks (its placement, the observer's first report)
  // must not be the ones that see the next DM.
  await settle(page);
}

async function closeThread(page: Page) {
  await page
    .locator("h2", { hasText: "Direct messages with" })
    .locator("xpath=following-sibling::button")
    .click();
  await expect(page.locator(THREAD)).toHaveCount(0);
}

/// A thread with `HISTORY` DMs, open and settled on its newest DM.
async function openThreadWithHistory(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  await callRiverTest(page, "appendDms", HISTORY);
  await openThread(page);
  await expect(dm(page, NEWEST_HISTORY)).toHaveCount(1, { timeout: 5_000 });
  await expectAtEnd(page, "opening the thread should land on its newest DM");
  const { max } = await threadGeometry(page);
  expect(max, "premise: the thread is long enough to scroll").toBeGreaterThan(300);
}

/// The thread's unread count in the DM rail. In the DOM even while the modal
/// covers it, and on phones while the rooms panel is shut.
function railBadge(page: Page) {
  return page.locator(".dm-rail-row-btn", { hasText: PEER }).getByTestId("dm-rail-unread-badge");
}

/// Let a patch, its effects and any deferred mark land, so that an unread
/// count read afterwards is not just early.
async function settle(page: Page) {
  await nextFrames(page);
  await page.waitForTimeout(300);
}

/// The (N) of the title while the tab is hidden: unread across every room and
/// DM thread. Hides the tab if it is not hidden already.
async function hiddenTitleCount(page: Page): Promise<number> {
  await setTabVisibility(page, "hidden");
  await settle(page);
  const counted = /^\((\d+)\) /.exec(await page.title());
  return counted ? Number(counted[1]) : 0;
}

function dm(page: Page, text: string) {
  return page.locator(THREAD).getByText(text, { exact: true });
}

/// Deliver an inbound DM and wait for it to be on the page. Attached, not
/// visible: it may land below the view, which is the point.
async function deliverDm(page: Page, text: string) {
  await callRiverTest(page, "deliverDm", text);
  await expect(dm(page, text)).toHaveCount(1, { timeout: 5_000 });
}

function threadGeometry(page: Page) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel)!;
    return {
      top: el.scrollTop,
      max: el.scrollHeight - el.clientHeight,
      toEnd: el.scrollHeight - el.scrollTop - el.clientHeight,
    };
  }, THREAD);
}

/// How far the bottom of the DM reading `text` (its bubble and timestamp) sits
/// below the thread's visible area. Negative: above its bottom edge.
function belowFold(page: Page, text: string) {
  return page.evaluate(
    ({ sel, text }) => {
      const thread = document.querySelector(sel)!;
      const body = Array.from(thread.querySelectorAll(".prose")).find(
        (el) => el.textContent?.trim() === text,
      );
      if (!body) throw new Error(`no DM reads ${JSON.stringify(text)}`);
      const message = body.parentElement!;
      return message.getBoundingClientRect().bottom - thread.getBoundingClientRect().bottom;
    },
    { sel: THREAD, text },
  );
}

async function expectAtEnd(page: Page, why: string) {
  await expect
    .poll(async () => (await threadGeometry(page)).toEnd, { timeout: 5_000, message: why })
    .toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
}

/// The reader scrolls the thread to `top` and the move lands. Native scrolling:
/// the thread has no gesture handling for a wheel or touch event to change.
async function readerScrollsTo(page: Page, top: number) {
  await page.evaluate(
    async ({ sel, top }) => {
      const el = document.querySelector(sel)!;
      if (Math.abs(el.scrollTop - top) < 1) return;
      const landed = new Promise((resolve) =>
        el.addEventListener("scroll", () => requestAnimationFrame(resolve), { once: true }),
      );
      el.scrollTop = top;
      await landed;
    },
    { sel: THREAD, top },
  );
}

/// Record every position the thread passes through from now on.
async function recordScrolls(page: Page) {
  await page.evaluate((sel) => {
    const el = document.querySelector(sel)!;
    const log = { start: el.scrollTop, seen: [] as number[] };
    (window as unknown as { __dmScrolls: typeof log }).__dmScrolls = log;
    el.addEventListener("scroll", () => log.seen.push(el.scrollTop));
  }, THREAD);
}

function recordedScrolls(page: Page) {
  return page.evaluate(
    () => (window as unknown as { __dmScrolls: { start: number; seen: number[] } }).__dmScrolls,
  );
}

/// Hold for a moment, then assert the thread never left where it was when
/// `recordScrolls` started, not even in passing.
async function expectStill(page: Page, why: string) {
  await page.waitForTimeout(600);
  const { start, seen } = await recordedScrolls(page);
  const { top } = await threadGeometry(page);
  const furthest = Math.max(0, ...[...seen, top].map((t) => Math.abs(t - start)));
  expect(furthest, why).toBeLessThanOrEqual(STILL_PX);
}

test.describe("DM thread scroll position (10c DM parity)", () => {
  test("opening a thread lands on its newest DM and marks it seen", async ({ page }) => {
    await openThreadWithHistory(page);
    expect(await belowFold(page, NEWEST_HISTORY)).toBeLessThanOrEqual(SLACK_PX);
    await expect(page.getByTestId(LATEST)).toBeHidden();
    await expect(railBadge(page), "opening the thread at its newest DM left it unread").toHaveCount(0);
  });

  test("an inbound DM at the end lands below the view and offers Latest", async ({ page }) => {
    await openThreadWithHistory(page);
    await recordScrolls(page);

    await deliverDm(page, "inbound at the end");

    await expectStill(page, "an inbound DM must not move the view, even at the end");
    expect(
      await belowFold(page, "inbound at the end"),
      "the inbound DM should sit below the view",
    ).toBeGreaterThan(SLACK_PX);
    await expect(page.getByTestId(LATEST)).toBeVisible();
  });

  // The one arrival the old 50px "near the bottom" rule followed. It measured
  // AFTER the arrival rendered, so a full DM row (~60px) at the end of a
  // scrolling thread was never within it; the arrival that first makes a short
  // thread scroll, by less than 50px, was.
  test("an inbound DM that makes a short thread scroll does not move the view", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "appendDms", 3);
    await openThread(page);
    await expect(dm(page, "dm history 02")).toHaveCount(1, { timeout: 5_000 });

    // Size the window so the modal, which grows with the thread up to 80vh,
    // has room for all but ~25px of one more DM.
    const fit = () =>
      page.evaluate((sel) => {
        const thread = document.querySelector(sel)!;
        const modal = thread.closest('[class~="max-h-[80vh]"]')!;
        const rows = thread.querySelectorAll(".prose");
        const last = rows[rows.length - 1].parentElement!;
        const prev = rows[rows.length - 2].parentElement!;
        return {
          room: parseFloat(getComputedStyle(modal).maxHeight) - modal.getBoundingClientRect().height,
          row: last.getBoundingClientRect().bottom - prev.getBoundingClientRect().bottom,
          toEnd: thread.scrollHeight - thread.scrollTop - thread.clientHeight,
        };
      }, THREAD);
    const before = await fit();
    const vp = page.viewportSize()!;
    await page.setViewportSize({
      width: vp.width,
      height: Math.round(vp.height + (before.row - 25 - before.room) / 0.8),
    });
    const tuned = await fit();
    expect(tuned.toEnd, "premise: the thread does not scroll yet").toBeLessThanOrEqual(0);
    const overflow = tuned.row - tuned.room;
    expect(overflow, "premise: one more DM overflows by more than the slack").toBeGreaterThan(SLACK_PX + 8);
    expect(overflow, "premise: ...and by less than the old 50px band").toBeLessThan(42);

    await recordScrolls(page);
    await deliverDm(page, "inbound that overflows");

    await expectStill(page, "an inbound DM must not move the view when it first makes the thread scroll");
    expect(await belowFold(page, "inbound that overflows")).toBeGreaterThan(SLACK_PX);
    await expect(page.getByTestId(LATEST)).toBeVisible();
  });

  test("an inbound DM does not move a reader parked up the thread", async ({ page }) => {
    await openThreadWithHistory(page);
    const { max } = await threadGeometry(page);
    await readerScrollsTo(page, Math.round(max / 2));
    await expect(page.getByTestId(LATEST)).toBeVisible();
    await recordScrolls(page);

    await deliverDm(page, "inbound while parked");

    await expectStill(page, "an inbound DM must not move a parked reader");
    await expect(page.getByTestId(LATEST)).toBeVisible();
  });

  test("an own send goes to the end once, and a later inbound DM does not follow it", async ({
    page,
  }) => {
    await openThreadWithHistory(page);
    await readerScrollsTo(page, 0);
    expect((await threadGeometry(page)).toEnd, "premise: parked at the top").toBeGreaterThan(300);

    const composer = page.getByPlaceholder("Type a direct message...");
    await composer.fill("sent from the top");
    await recordScrolls(page);
    await composer.press("Enter");

    await expect(dm(page, "sent from the top")).toHaveCount(1, { timeout: 5_000 });
    // Instant, like the room's (decision 12): the first position the thread
    // reports passing through is already the end, and so is the first read
    // after it.
    await expect
      .poll(async () => (await recordedScrolls(page)).seen.length, { message: "the send never scrolled" })
      .toBeGreaterThan(0);
    const firstRead = await threadGeometry(page);
    const { seen } = await recordedScrolls(page);
    expect(Math.abs(seen[0] - firstRead.max), "an own send should jump, not animate").toBeLessThanOrEqual(STILL_PX);
    expect(firstRead.toEnd, "an own send should jump, not animate").toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
    expect(await belowFold(page, "sent from the top")).toBeLessThanOrEqual(SLACK_PX);
    await expect(page.getByTestId(LATEST)).toBeHidden();

    await recordScrolls(page);
    await deliverDm(page, "inbound after the send");
    await expectStill(page, "the send's scroll must not turn into following");
    await expect(page.getByTestId(LATEST)).toBeVisible();
  });

  test("reopening a thread lands on its newest DM", async ({ page }) => {
    await openThreadWithHistory(page);
    await readerScrollsTo(page, 0);
    await closeThread(page);
    // The thread is closed, so the rail's count is the only thing to wait on.
    await callRiverTest(page, "deliverDm", "arrived while closed");
    await expect(railBadge(page), "premise: a DM to a closed thread is unread").toHaveText("1");

    await openThread(page);

    await expect(dm(page, "arrived while closed")).toHaveCount(1, { timeout: 5_000 });
    await expectAtEnd(page, "reopening the thread should land on its newest DM");
    expect(await belowFold(page, "arrived while closed")).toBeLessThanOrEqual(SLACK_PX);
    await expect(page.getByTestId(LATEST)).toBeHidden();
    await expect(railBadge(page), "reopening the thread at its newest DM left it unread").toHaveCount(0);
  });

  test("Latest shows once the newest DM's bottom is more than a few px off screen", async ({
    page,
  }) => {
    await openThreadWithHistory(page);
    const { max } = await threadGeometry(page);
    const atEnd = await belowFold(page, NEWEST_HISTORY);

    // Scrolled up until the newest DM's bottom is 16px under the fold.
    await readerScrollsTo(page, max - (SLACK_PX + 12 - atEnd));
    expect(await belowFold(page, NEWEST_HISTORY)).toBeGreaterThan(SLACK_PX + 8);
    await expect(page.getByTestId(LATEST)).toBeVisible();

    // Its bottom back inside the view (2px up, short of the end): no Latest.
    await readerScrollsTo(page, max - (-2 - atEnd));
    expect(await belowFold(page, NEWEST_HISTORY)).toBeLessThanOrEqual(0);
    await expect(page.getByTestId(LATEST)).toBeHidden();
  });

  test("Latest jumps to the end instantly, then hides and does not follow", async ({ page }) => {
    await openThreadWithHistory(page);
    await readerScrollsTo(page, 0);
    const latest = page.getByTestId(LATEST);
    await expect(latest).toBeVisible();
    await recordScrolls(page);

    await latest.click();

    // Instant: the first read after the click is already the end, and so is
    // the first position the thread reported passing through. (Scroll events
    // fire at the next frame, hence the wait before reading them.)
    expect((await threadGeometry(page)).toEnd, "Latest should jump, not animate").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    const { seen } = await recordedScrolls(page);
    const { max } = await threadGeometry(page);
    expect(seen.length, "Latest should have scrolled").toBeGreaterThan(0);
    expect(Math.abs(seen[0] - max), "Latest should jump, not animate").toBeLessThanOrEqual(STILL_PX);
    await expect(latest).toBeHidden();

    await recordScrolls(page);
    await deliverDm(page, "inbound after Latest");
    await expectStill(page, "Latest must not turn into following");
    await expect(latest).toBeVisible();
  });
});

// 10c decision 11: an open thread is marked seen only up to an inbound DM that
// was on screen with the tab visible. Before 10c the thread marked every
// inbound DM seen as it rendered, wherever it landed. Witnessed by the rail's
// unread count for the thread and the hidden-tab title, which counts DMs.
test.describe("DM thread read rule (10c decision 11)", () => {
  test("an inbound DM below the fold stays unread until the reader scrolls to it", async ({ page }) => {
    await openThreadWithHistory(page);
    await expect(railBadge(page), "premise: opening the thread marked it seen").toHaveCount(0);

    await deliverDm(page, "unseen below the fold");
    expect(await belowFold(page, "unseen below the fold"), "premise: the DM lands below the view").toBeGreaterThan(
      SLACK_PX,
    );
    await settle(page);
    await expect(railBadge(page), "a DM that landed below the fold was marked seen").toHaveText("1");

    const unseen = await hiddenTitleCount(page);
    await setTabVisibility(page, "visible");
    await settle(page);
    expect(await belowFold(page, "unseen below the fold"), "premise: the reader has not moved").toBeGreaterThan(
      SLACK_PX,
    );
    await expect(railBadge(page), "the tab coming back marked a DM below the fold seen").toHaveText("1");

    await readerScrollsTo(page, (await threadGeometry(page)).max);
    await expect(railBadge(page), "the reader reached the DM and the thread still counts it unread").toHaveCount(0);
    expect(await hiddenTitleCount(page), "the hidden-tab title still counts the DM the reader reached").toBe(
      unseen - 1,
    );
  });

  test("Latest marks a DM below the fold seen", async ({ page }) => {
    await openThreadWithHistory(page);
    await deliverDm(page, "unseen until Latest");
    await settle(page);
    await expect(railBadge(page), "premise: the DM below the fold is unread").toHaveText("1");

    await page.getByTestId(LATEST).click();

    await expectAtEnd(page, "premise: Latest reached the end");
    await expect(railBadge(page), "Latest brought the DM on screen and the thread still counts it unread").toHaveCount(0);
  });

  test("an inbound DM that lands on screen with the tab visible is marked seen", async ({ page }) => {
    await openShortThread(page);
    await expect(railBadge(page), "premise: opening the thread marked it seen").toHaveCount(0);

    await deliverDm(page, "on screen");
    expect(await belowFold(page, "on screen"), "premise: all of the DM is on screen").toBeLessThanOrEqual(0);
    await settle(page);

    await expect(railBadge(page), "a DM that landed on screen stayed unread").toHaveCount(0);
  });

  test("an inbound DM on screen in a hidden tab stays unread until the tab is visible", async ({ page }) => {
    await openShortThread(page);
    await expect(railBadge(page), "premise: opening the thread marked it seen").toHaveCount(0);
    const before = await hiddenTitleCount(page);

    await deliverDm(page, "on screen in a hidden tab");
    expect(await belowFold(page, "on screen in a hidden tab"), "premise: all of the DM is on screen").toBeLessThanOrEqual(
      0,
    );
    await settle(page);
    await expect(railBadge(page), "a DM seen only in a hidden tab was marked seen").toHaveText("1");
    expect(await hiddenTitleCount(page), "the hidden-tab title does not count the DM").toBe(before + 1);

    await setTabVisibility(page, "visible");
    await expect(
      railBadge(page),
      "the tab came back with the DM on screen and the thread still counts it unread",
    ).toHaveCount(0);
  });
});
