import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { selectRoom } from "./example-room";
import { newestVisibleRow, registerHistoryGeometry, type RowPosition } from "./history-scroll-geometry";
import {
  afterObserverAndTask,
  expectEmptyButVisible,
  registerEmptyRender,
  restoreHistory,
} from "./history-empty-render-fixture";
import {
  AT_BOTTOM_EPSILON_PX,
  DEEP_ROOM_PATH,
  afterLayoutSettles,
  backfillOnce,
  deliver,
  distanceFromBottom,
  expectSettledAtBottom,
  expectVisibleRowHolds,
  maxScrollTop,
  openRoomAtBottom,
  readerScrollsToEnd,
  readerScrollsWithoutGesture,
  renderedRowCount,
} from "./history-scroll-helpers";

const DEEP_ROOM = "Deep History Room";
/// A fixture premise: placement may compensate layout movement up to the model's allowance.
const LAYOUT_SHIFT_ALLOWANCE_PX = 200;
const HISTORY_ITEMS = '[data-testid="conversation-history"] > [data-item-key]';
type PlacementWrite = { items: number; move: number };

type DeferredProbe = {
  /// Intercept zero-delay timers only while the synchronous step runs.
  captureEnd(hold: boolean): { made: number; atEnd: boolean; reading: RowPosition | null };
  release(): void;
  log: string[];
};

declare global {
  interface Window {
    __riverDeferred?: DeferredProbe;
  }
}

/// Install the local probe in the page. The original timer is restored before
/// control returns to Playwright, including when the captured step throws.
function installDeferredProbe() {
  const schedule = window.setTimeout.bind(window);
  const log: string[] = [];
  let held: (() => void)[] = [];
  function capture(step: () => void, hold: boolean) {
    const original = window.setTimeout;
    let made = 0;
    window.setTimeout = function (callback: unknown, delay?: number, ...args: unknown[]) {
      if (typeof callback !== "function" || (delay ?? 0) !== 0) {
        return (original as (...values: unknown[]) => number)(callback, delay, ...args);
      }
      made++;
      const run = () => {
        log.push("deferred");
        (callback as (...values: unknown[]) => unknown)(...args);
      };
      if (!hold) return schedule(run, 0);
      held.push(run);
      return 0;
    } as typeof window.setTimeout;
    try {
      step();
    } finally {
      window.setTimeout = original;
    }
    return made;
  }
  window.__riverDeferred = {
    log,
    captureEnd(hold) {
      const container = document.getElementById("chat-scroll-container")!;
      let atEnd = false;
      let reading: RowPosition | null = null;
      const made = capture(() => {
        const max = container.scrollHeight - container.clientHeight;
        container.scrollTop = max;
        container.dispatchEvent(new Event("scroll"));
        atEnd = max - container.scrollTop <= 2;
        reading = window.__riverHistoryGeometry!.newestVisible(container);
      }, hold);
      return { made, atEnd, reading };
    },
    release() {
      const runs = held;
      held = [];
      for (const run of runs) schedule(run, 0);
    },
  };
}

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
  await registerEmptyRender(page);
  await page.addInitScript(installDeferredProbe);
});

/// Display items exclude date separators, which can change when a trim starts
/// the window on another day.
function renderedItemCount(page: Page): Promise<number> {
  return page.locator(HISTORY_ITEMS).count();
}

/// Open the deep room and page back once, so the window is well past its
/// initial size. Returns the rendered row count after the backfill.
async function openBackfilled(page: Page) {
  await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
  await expect(page.locator("#top-backfill-sentinel"), "premise: the room is windowed").toHaveCount(1);
  return backfillOnce(page, "premise: reaching the top backfills");
}

/// Mark the first rendered item so a trim that removes the head is observable.
function tagHead(page: Page) {
  return page.evaluate(() => {
    const head = document.querySelector<HTMLElement>(
      '[data-testid="conversation-history"] > [data-item-key]',
    )!;
    (head as unknown as { __riverTrimProbe: boolean }).__riverTrimProbe = true;
  });
}

function headStillRendered(page: Page) {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('[data-testid="conversation-history"] > [data-item-key]')).some(
      (row) => (row as unknown as { __riverTrimProbe?: boolean }).__riverTrimProbe,
    ),
  );
}

/// The last step's callbacks have run and their renders have landed.
async function afterQueuedCallbacks(page: Page) {
  await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 0)));
  await afterLayoutSettles(page);
}

/// Common evidence that a rejected callback left the grown history intact.
async function expectWindowPreserved(
  page: Page,
  grown: number,
  why: string,
  headIsPresent: Promise<boolean> = headStillRendered(page),
) {
  expect(await headIsPresent, `${why}: the original head was removed`).toBe(true);
  expect(await renderedRowCount(page), `${why}: the window shrank`).toBeGreaterThanOrEqual(grown);
}

function expectCapturedEnd(race: { made: number; atEnd: boolean; reading: RowPosition | null }) {
  expect(race.atEnd, "premise: the move reached the end").toBe(true);
  expect(race.made, "premise: the end queued deferred work").toBeGreaterThan(0);
  expect(race.reading, "premise: a message was visible at the end").not.toBeNull();
}

/// A genuine return to the end must still be eligible to trim after a rejected
/// callback.
async function expectALaterReturnTrims(page: Page, grown: number) {
  await readerScrollsWithoutGesture(page, Math.round((await maxScrollTop(page)) / 2));
  await afterLayoutSettles(page);
  await readerScrollsToEnd(page);
  await expect
    .poll(() => renderedRowCount(page), {
      timeout: 5_000,
      message: "a genuine return to the end did not trim: the rejected trim used up its eligibility",
    })
    .toBeLessThan(Math.min(67, grown));
}

test.describe("A bottom trim is decided again when it runs", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("multiple queued callbacks apply at most one bottom trim", async ({ page }) => {
    const grown = await openBackfilled(page);
    await tagHead(page);
    const queued = await page.evaluate(() => {
      const container = document.getElementById("chat-scroll-container")!;
      const first = window.__riverDeferred!.captureEnd(true);
      // Make a real reader-position transition before returning to the end;
      // otherwise the second scroll observation is an echo and queues nothing.
      container.scrollTop = Math.round((container.scrollHeight - container.clientHeight) / 2);
      container.dispatchEvent(new Event("scroll"));
      const second = window.__riverDeferred!.captureEnd(true);
      return [first, second];
    });
    expect(queued, "premise: both end observations queued deferred work").toHaveLength(2);
    for (const race of queued) expectCapturedEnd(race);

    await page.evaluate(() => window.__riverDeferred!.release());
    await expect
      .poll(() => renderedItemCount(page), {
        timeout: 5_000,
        message: "the first eligible callback did not trim the overgrown window",
      })
      .toBeLessThan(grown);
    await afterQueuedCallbacks(page);

    const remaining = await renderedItemCount(page);
    expect(remaining, "the queued callbacks trimmed past the normal retained window").toBeGreaterThanOrEqual(40);
    expect(remaining, "the overgrown window should be trimmed once to its normal size").toBeLessThan(67);
    expect(await headStillRendered(page), "the first eligible callback left the old head in place").toBe(false);

    // The normal cap remains pageable after the queued callbacks have drained.
    await page.evaluate(() => {
      document.getElementById("chat-scroll-container")!.scrollTop = 0;
    });
    await expect
      .poll(() => renderedItemCount(page), {
        timeout: 5_000,
        message: "backfill after the queued trim revealed no older history",
      })
      .toBeGreaterThan(remaining + 40);
  });

  test("a reader who leaves the end before the trim runs keeps the backfilled rows", async ({ page }) => {
    const grown = await openBackfilled(page);
    await tagHead(page);
    // The end queues a trim, then the reader moves well back before that task.
    const race = await page.evaluate(() => {
      const container = document.getElementById("chat-scroll-container")!;
      const max = container.scrollHeight - container.clientHeight;
      container.scrollTop = max;
      container.dispatchEvent(new Event("scroll"));
      const atEnd = max - container.scrollTop <= 2;
      container.scrollTop = Math.round(max * 0.3);
      container.dispatchEvent(new Event("scroll"));
      return {
        atEnd,
        back: container.scrollTop,
        max,
        reading: window.__riverHistoryGeometry!.newestVisible(container),
      };
    });
    expect(race.atEnd, "premise: the first move reached the end").toBe(true);
    expect(race.max - race.back, "premise: the second move went well back").toBeGreaterThan(2_000);
    expect(race.reading, "premise: a message is visible where the reader went").not.toBeNull();

    await afterQueuedCallbacks(page);
    await expectWindowPreserved(page, grown, "the queued trim");
    await expectVisibleRowHolds(page, race.reading!, "the queued trim moved the reader's new message");
    await expectALaterReturnTrims(page, grown);
  });

  test("a trim queued just before the history is hidden does not run while it is hidden", async ({ page }) => {
    const grown = await openBackfilled(page);
    await tagHead(page);
    try {
      const atEnd = await page.evaluate(() => {
        const container = document.getElementById("chat-scroll-container")!;
        const max = container.scrollHeight - container.clientHeight;
        container.scrollTop = max;
        container.dispatchEvent(new Event("scroll"));
        const atEnd = max - container.scrollTop <= 2;
        container.style.display = "none";
        return atEnd;
      });
      expect(atEnd, "premise: the move reached the end").toBe(true);
      await afterQueuedCallbacks(page);
      expect(
        await page.evaluate(() => document.getElementById("chat-scroll-container")!.clientHeight),
        "premise: the history is hidden",
      ).toBe(0);
      expect(await headStillRendered(page), "the queued trim ran while the history was hidden").toBe(true);
    } finally {
      await page.evaluate(() =>
        document.getElementById("chat-scroll-container")?.style.removeProperty("display"),
      );
    }
    await afterLayoutSettles(page);
    await expectWindowPreserved(page, grown, "the hidden-history trim");
    await expectALaterReturnTrims(page, grown);
  });

  test("a trim queued just before the rows stop rendering does not run while the visible history has none", async ({
    page,
  }) => {
    const grown = await openBackfilled(page);
    // Settle just short of the end so the scroll-to-latest observer has run
    // before rows are detached.
    await readerScrollsWithoutGesture(page, (await maxScrollTop(page)) - 10);
    await afterLayoutSettles(page);
    await tagHead(page);
    let mutations: number | null = null;
    let reading: RowPosition | null = null;
    try {
      const race = await page.evaluate(() => {
        const end = window.__riverDeferred!.captureEnd(false);
        const sizes = window.__riverEmptyRender!.empty(true);
        window.__riverDeferred!.log.push("emptied");
        return { ...end, ...sizes };
      });
      expectCapturedEnd(race);
      reading = race.reading;
      expect(Math.abs(race.after - race.before), "premise: the placeholder keeps the height").toBeLessThanOrEqual(1);
      await afterObserverAndTask(page);
      await expectEmptyButVisible(page);
      const log = await page.evaluate(() => window.__riverDeferred!.log);
      expect(log[0], `premise: rows disappeared before queued callbacks ran (${log})`).toBe("emptied");
      expect(log, "premise: the queued callbacks have run").toContain("deferred");
      mutations = await restoreHistory(page);
    } finally {
      await restoreHistory(page);
    }
    expect(mutations, "the queued trim re-rendered the history while it had no rows").toBe(0);
    await afterLayoutSettles(page);
    await expectWindowPreserved(page, grown, "the rows-disappeared trim");
    await expectVisibleRowHolds(page, reading!, "the rows came back somewhere other than the reader's message");
    await expectALaterReturnTrims(page, grown);
  });

  test("a trim queued just before a room switch does not run in the room current by then", async ({ page }) => {
    const grown = await openBackfilled(page);
    const headKey = await page.locator(HISTORY_ITEMS).first().getAttribute("data-item-key");
    expect(headKey, "premise: the window's first item has a key").not.toBeNull();
    const headRendered = async () =>
      (await page.locator(`${HISTORY_ITEMS}[data-item-key="${headKey}"]`).count()) > 0;
    const race = await page.evaluate(() => window.__riverDeferred!.captureEnd(true));
    expectCapturedEnd(race);

    // Away and back makes the visit guard necessary: on return this room is
    // again overgrown and at its end, so the other trim checks would pass.
    await selectRoom(page, "Capped History Room");
    await expect.poll(headRendered, { message: "premise: the other room rendered" }).toBe(false);
    await afterLayoutSettles(page);
    await selectRoom(page, DEEP_ROOM);
    await expect.poll(headRendered, { message: "premise: the room came back with its grown window" }).toBe(true);
    await afterLayoutSettles(page);
    await expectVisibleRowHolds(page, race.reading!, "premise: the room came back at the reader's message");
    expect(await distanceFromBottom(page), "premise: the room came back at its end").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    expect(await headRendered(), "premise: the restored window is intact before the stale callback").toBe(true);

    await page.evaluate(() => {
      window.__riverDeferred!.log.push("switched");
      window.__riverDeferred!.release();
    });
    await afterQueuedCallbacks(page);
    const log = await page.evaluate(() => window.__riverDeferred!.log);
    expect(log[0], `premise: the room switched before queued callbacks ran (${log})`).toBe("switched");
    expect(log, "premise: the queued callbacks have run").toContain("deferred");
    await expectWindowPreserved(page, grown, "the stale trim", headRendered());
    await expectVisibleRowHolds(page, race.reading!, "the stale trim moved the reader's message");
    await expectALaterReturnTrims(page, grown);
  });
});

test.describe("A placement at the latest message trims", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("deleting the anchor of a grown window lands at the latest message and trims, with a small move", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expect(page.locator("#top-backfill-sentinel"), "premise: the room is windowed").toHaveCount(1);
    await afterLayoutSettles(page);
    const initial = await renderedItemCount(page);
    await deliver(page, "short arrival one");
    await deliver(page, "short arrival two");
    await afterLayoutSettles(page);
    expect(await renderedItemCount(page), "premise: the arrivals grew the window").toBeGreaterThanOrEqual(initial + 2);
    const anchor = await newestVisibleRow(page);
    expect(anchor, "premise: a message is visible").not.toBeNull();
    expect(anchor!.id, "premise: the anchor is not an arrival").not.toMatch(/arrival/);
    expect(
      await page.evaluate((id) => document.getElementById(id)!.textContent, anchor!.id),
      "premise: the anchor is an old message",
    ).not.toContain("short arrival");

    // Record only the values used to identify placement, its size, and its
    // scroll movement; delegate each write to the native setter unchanged.
    await page.evaluate(() => {
      const container = document.getElementById("chat-scroll-container")!;
      const native = Object.getOwnPropertyDescriptor(Element.prototype, "scrollTop")!;
      const placements: PlacementWrite[] = [];
      (window as unknown as { __riverScrollTopPlacements: PlacementWrite[] }).__riverScrollTopPlacements =
        placements;
      Object.defineProperty(container, "scrollTop", {
        configurable: true,
        get() {
          return native.get!.call(this);
        },
        set(value: number) {
          const before = native.get!.call(this) as number;
          const scrollHeight = this.scrollHeight;
          const items = document.querySelectorAll(
            '[data-testid="conversation-history"] > [data-item-key]',
          ).length;
          native.set!.call(this, value);
          if (value === scrollHeight) {
            const after = native.get!.call(this) as number;
            placements.push({ items, move: Math.abs(after - before) });
          }
        },
      });
    });
    let placements: PlacementWrite[] = [];
    try {
      const unmatched = await callRiverTest(page, "removeMessages", [anchor!.id]);
      expect(unmatched, "premise: the anchor named a message").toEqual([]);
      await expectSettledAtBottom(page, "deleting the anchor should land at the latest message");
    } finally {
      placements = await page.evaluate(() => {
        delete (document.getElementById("chat-scroll-container") as unknown as { scrollTop?: number }).scrollTop;
        return (window as unknown as { __riverScrollTopPlacements: PlacementWrite[] }).__riverScrollTopPlacements;
      });
    }
    const placement = placements[0];
    const what = `initial ${initial} items; placement writes ${JSON.stringify(placements)}`;
    expect(placement, `premise: the deletion placed at the latest message (${what})`).toBeDefined();
    expect(
      placement!.items,
      `premise: the window was still overgrown at placement, before any trim (${what})`,
    ).toBeGreaterThan(initial);
    expect(
      placement!.move,
      `premise: placement moved no more than the layout allowance (${what})`,
    ).toBeLessThanOrEqual(LAYOUT_SHIFT_ALLOWANCE_PX);
    const landed = await newestVisibleRow(page);
    expect(landed, "premise: a message is visible at the landing").not.toBeNull();

    await expect
      .poll(() => renderedItemCount(page), {
        timeout: 5_000,
        message: "the placement at the latest message did not trim the overgrown window",
      })
      .toBeLessThanOrEqual(initial);
    await expectVisibleRowHolds(page, landed!, "the trim moved the landing");
  });
});
