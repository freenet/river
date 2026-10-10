import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom, setTabVisibility, hiddenTitleCount } from "./example-room";
import { AT_BOTTOM_EPSILON_PX, NEWEST_IN_VIEW_SLACK_PX, nextFrames, settle } from "./history-geometry";

// Where a DM thread's view goes: opening it lands on the
// newest DM, an own send jumps to the end once, and Latest jumps there on
// request. Nothing else moves the view. An inbound DM never does, even when
// the reader is at the end; it lands below and Latest offers it.
//
// And when the thread counts as seen: only
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

/// Open Team Chat, seed `count` DMs ("dm history 00" onwards), and open their
/// thread once the newest of them has rendered.
async function prepareThread(page: Page, count: number) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  await callRiverTest(page, "appendDms", count);
  await openThread(page);
  const newest = `dm history ${String(count - 1).padStart(2, "0")}`;
  await expect(dm(page, newest)).toHaveCount(1, { timeout: 5_000 });
}

/// A thread too short to scroll ("dm history 00" to "02"), open and settled,
/// so the next inbound DM lands on screen and is the only thing that changes.
async function openShortThread(page: Page) {
  await prepareThread(page, 3);
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
  await prepareThread(page, HISTORY);
  await expectAtEnd(page, "opening the thread should land on its newest DM");
  const { max } = await threadGeometry(page);
  expect(max, "premise: the thread is long enough to scroll").toBeGreaterThan(300);
}

/// The thread's unread count in the DM rail. In the DOM even while the modal
/// covers it, and on phones while the rooms panel is shut.
function railBadge(page: Page) {
  return page.locator(".dm-rail-row-btn", { hasText: PEER }).getByTestId("dm-rail-unread-badge");
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

/// Uncaught page errors and console errors from now on: a WASM panic surfaces
/// as either.
function recordPageErrors(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  return errors;
}

function roomReadFailuresTaken(page: Page) {
  return callRiverTest(page, "dmRoomReadFailuresTaken");
}

/// Make the open thread's next `ROOMS` read fail, as a contended one does, and
/// wait until it has and the retry it nudged has rendered.
async function failNextRoomRead(page: Page) {
  const before = await roomReadFailuresTaken(page);
  await callRiverTest(page, "failNextDmRoomRead");
  await expect
    .poll(() => roomReadFailuresTaken(page), { message: "the forced room-read failure was never taken" })
    .toBe(before + 1);
  await settle(page);
}

/// Remember the thread's scroller and composer elements, to check later that
/// they are the same nodes rather than replacements.
async function rememberThreadElements(page: Page) {
  await page.evaluate((sel) => {
    const w = window as unknown as Record<string, Element | null>;
    w.__dmScroller = document.querySelector(sel);
    w.__dmComposer = document.querySelector('textarea[placeholder="Type a direct message..."]');
  }, THREAD);
}

function sameThreadElements(page: Page) {
  return page.evaluate((sel) => {
    const w = window as unknown as Record<string, Element | null>;
    return {
      scroller: w.__dmScroller !== null && w.__dmScroller === document.querySelector(sel),
      composer:
        w.__dmComposer !== null &&
        w.__dmComposer === document.querySelector('textarea[placeholder="Type a direct message..."]'),
    };
  }, THREAD);
}

test.describe("DM thread scroll position", () => {
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
    // Instant, like the room's: the first position the thread
    // reports passing through is already the end, and so is the first read
    // after it.
    await expect
      .poll(async () => (await recordedScrolls(page)).seen.length, { message: "the send never scrolled" })
      .toBeGreaterThan(0);
    const firstRead = await threadGeometry(page);
    const { seen } = await recordedScrolls(page);
    expect(Math.abs(seen[0] - firstRead.max), "an own send should jump, not animate").toBeLessThanOrEqual(STILL_PX);
    expect(firstRead.toEnd, "an own send should jump, not animate").toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
    expect(await belowFold(page, "sent from the top")).toBeLessThanOrEqual(NEWEST_IN_VIEW_SLACK_PX);
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
    expect(await belowFold(page, "arrived while closed")).toBeLessThanOrEqual(NEWEST_IN_VIEW_SLACK_PX);
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
    await readerScrollsTo(page, max - (NEWEST_IN_VIEW_SLACK_PX + 12 - atEnd));
    expect(await belowFold(page, NEWEST_HISTORY)).toBeGreaterThan(NEWEST_IN_VIEW_SLACK_PX + 8);
    await expect(page.getByTestId(LATEST)).toBeVisible();

    // Its bottom back inside the view (2px up, short of the end): no Latest.
    await readerScrollsTo(page, max - (-2 - atEnd));
    expect(await belowFold(page, NEWEST_HISTORY)).toBeLessThanOrEqual(0);
    await expect(page.getByTestId(LATEST)).toBeHidden();
  });

  test("Latest jumps to the end instantly, marks the DM below the fold seen, then hides and does not follow", async ({
    page,
  }) => {
    await openThreadWithHistory(page);
    await readerScrollsTo(page, 0);
    const latest = page.getByTestId(LATEST);
    await expect(latest).toBeVisible();
    await deliverDm(page, "unseen until Latest");
    await settle(page);
    await expect(railBadge(page), "premise: the DM below the fold is unread").toHaveText("1");
    await recordScrolls(page);

    await latest.click();

    // Instant: the first read after the click is already the end, and so is
    // the first position the thread reported passing through. (Scroll events
    // fire at the next frame, hence the wait before reading them.)
    expect((await threadGeometry(page)).toEnd, "Latest should jump, not animate").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    await nextFrames(page);
    const { seen } = await recordedScrolls(page);
    const { max } = await threadGeometry(page);
    expect(seen.length, "Latest should have scrolled").toBeGreaterThan(0);
    expect(Math.abs(seen[0] - max), "Latest should jump, not animate").toBeLessThanOrEqual(STILL_PX);
    await expect(latest).toBeHidden();
    await expect(railBadge(page), "Latest brought the DM on screen and the thread still counts it unread").toHaveCount(0);

    await recordScrolls(page);
    await deliverDm(page, "inbound after Latest");
    await expectStill(page, "Latest must not turn into following");
    await expect(latest).toBeVisible();
  });
});

// An open thread is marked seen only up to an inbound DM that
// was on screen with the tab visible. Previously the thread marked every
// inbound DM seen as it rendered, wherever it landed. Witnessed by the rail's
// unread count for the thread and the hidden-tab title, which counts DMs.
// Hook-driven, so it runs on one engine; the describe above covers the view on
// every engine.
test.describe("DM thread read rule", { tag: "@chromium-only" }, () => {
  test("an inbound DM at the end lands below the view, offers Latest, and stays unread until the reader scrolls to it", async ({
    page,
  }) => {
    await openThreadWithHistory(page);
    await expect(railBadge(page), "premise: opening the thread marked it seen").toHaveCount(0);
    await recordScrolls(page);

    await deliverDm(page, "unseen below the fold");
    await expectStill(page, "an inbound DM must not move the view, even at the end");
    expect(await belowFold(page, "unseen below the fold"), "the inbound DM should sit below the view").toBeGreaterThan(
      NEWEST_IN_VIEW_SLACK_PX,
    );
    await expect(page.getByTestId(LATEST)).toBeVisible();
    await settle(page);
    await expect(railBadge(page), "a DM that landed below the fold was marked seen").toHaveText("1");

    const unseen = await hiddenTitleCount(page);
    await setTabVisibility(page, "visible");
    await settle(page);
    expect(await belowFold(page, "unseen below the fold"), "premise: the reader has not moved").toBeGreaterThan(
      NEWEST_IN_VIEW_SLACK_PX,
    );
    await expect(railBadge(page), "the tab coming back marked a DM below the fold seen").toHaveText("1");

    await readerScrollsTo(page, (await threadGeometry(page)).max);
    await expect(railBadge(page), "the reader reached the DM and the thread still counts it unread").toHaveCount(0);
    expect(await hiddenTitleCount(page), "the hidden-tab title still counts the DM the reader reached").toBe(
      unseen - 1,
    );
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

  test("an inbound DM on screen under the thread's confirmation stays unread until it closes, which does not scroll", async ({
    page,
  }) => {
    await openShortThread(page);
    await page.getByRole("button", { name: "Delete their messages" }).click();
    await expect(page.getByRole("dialog", { name: "Confirm delete their messages" })).toBeVisible();
    await recordScrolls(page);

    await deliverDm(page, "on screen under a confirmation");
    expect(await belowFold(page, "on screen under a confirmation"), "premise: all of the DM is on screen").toBeLessThanOrEqual(
      0,
    );
    await settle(page);
    await expect(railBadge(page), "a DM seen only under a modal was marked seen").toHaveText("1");

    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(railBadge(page), "closing the confirmation left the DM on screen unread").toHaveCount(0);
    await expectStill(page, "closing the confirmation moved the thread");
  });
});

// A failed `ROOMS` read (contention) is transient: the open thread keeps what
// it last showed until the retry succeeds. A room that is gone is not.
test.describe("DM thread room read", () => {
  test("a failed room read keeps the open thread, its draft and the reader's place", async ({ page }) => {
    const errors = recordPageErrors(page);
    await openThreadWithHistory(page);
    await readerScrollsTo(page, Math.round((await threadGeometry(page)).max / 2));
    const composer = page.getByPlaceholder("Type a direct message...");
    await composer.fill("unsent draft");
    await deliverDm(page, "unread below the fold");
    await settle(page);
    await expect(railBadge(page), "premise: the DM below the fold is unread").toHaveText("1");
    await rememberThreadElements(page);
    await recordScrolls(page);

    await failNextRoomRead(page);

    expect(await sameThreadElements(page), "a failed room read replaced the thread").toEqual({
      scroller: true,
      composer: true,
    });
    await expectStill(page, "a failed room read moved the reader");
    await expect(composer).toHaveValue("unsent draft");
    await expect(railBadge(page), "a failed room read marked the DM below the fold seen").toHaveText("1");

    // The retry re-subscribed the thread to the room.
    await deliverDm(page, "after the failed read");
    expect(await sameThreadElements(page)).toEqual({ scroller: true, composer: true });
    await expect(composer).toHaveValue("unsent draft");
    expect(errors).toEqual([]);
  });

  test("a room read that fails as the thread opens recovers and lands on the newest DM", async ({ page }) => {
    const errors = recordPageErrors(page);
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "appendDms", HISTORY);
    // Pending until a thread reads the room: none is open yet.
    await callRiverTest(page, "failNextDmRoomRead");
    expect(await roomReadFailuresTaken(page), "premise: no thread has read the room yet").toBe(0);

    await openThread(page);

    await expect.poll(() => roomReadFailuresTaken(page), { message: "the opening read never failed" }).toBe(1);
    await expect(dm(page, NEWEST_HISTORY)).toHaveCount(1, { timeout: 5_000 });
    await expectAtEnd(page, "a thread whose first read failed should still open on its newest DM");
    await expect(page.getByTestId("dm-thread-unavailable")).toHaveCount(0);
    await expect(page.getByTestId(LATEST)).toBeHidden();
    expect(errors).toEqual([]);
  });

  test("a room that is gone replaces the open thread with the unavailable notice", async ({ page }) => {
    const errors = recordPageErrors(page);
    await openThreadWithHistory(page);

    // Clears every room, so the thread's room is genuinely absent.
    await callRiverTest(page, "setRoomsLoadState", "loaded");

    await expect(page.getByTestId("dm-thread-unavailable")).toHaveCount(1, { timeout: 5_000 });
    await expect(page.locator(THREAD)).toHaveCount(0);
    await expect(page.getByText(NEWEST_HISTORY, { exact: true })).toHaveCount(0);
    expect(errors).toEqual([]);
  });
});
