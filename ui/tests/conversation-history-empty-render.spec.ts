import { test, expect, Page } from "@playwright/test";
import { registerHistoryGeometry, type RowPosition } from "./history-scroll-geometry";
import {
  ARRIVAL,
  AT_BOTTOM_EPSILON_PX,
  PARKED_ABOVE_END_PX,
  afterLayoutSettles,
  animationUnderway,
  deliver,
  distanceFromBottom,
  expectVisibleRowHolds,
  maxScrollTop,
  parkAboveTheEnd,
  readerScrollsToEnd,
  recordScrollRequests,
  scrollRequests,
  scrollTop,
  viewAtRest,
} from "./history-scroll-helpers";
import {
  afterObserverAndTask,
  emptyHistory,
  expectEmptyButVisible,
  registerEmptyRender,
  restoreHistory,
  visibleRow,
} from "./history-empty-render-fixture";

const PARK_PX = 3_000;
/// The scroll model's layout-movement allowance; this is only a fixture premise.
const LAYOUT_SHIFT_ALLOWANCE_PX = 200;
const button = (page: Page) => page.getByTestId("scroll-to-bottom");

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
  await registerEmptyRender(page);
});

test.describe("A render with no rows is not a deleted anchor", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("restore: the parked message comes back at its gap when the rows return", async ({ page }) => {
    await parkAboveTheEnd(page, PARK_PX);
    const saved = await visibleRow(page);
    try {
      const { before, after } = await emptyHistory(page, { keepHeight: false });
      expect(after, "premise: the empty render is much shorter than the history").toBeLessThan(before / 4);
      await expectEmptyButVisible(page);
      await afterObserverAndTask(page);
      await expectEmptyButVisible(page);
    } finally {
      await restoreHistory(page);
    }
    expect(await page.locator("#chat-content .anchor-row").count(), "premise: the rows are back").toBeGreaterThan(0);
    await expectVisibleRowHolds(page, saved!, "the rows came back somewhere other than the reader's message");
    expect(await distanceFromBottom(page), "the reader was sent to the latest message").toBeGreaterThan(
      PARKED_ABOVE_END_PX,
    );
  });

  for (const reader of ["parked", "at the end"] as const) {
    test(`the synchronous empty-layout clamp is consumed for a reader ${reader}`, async ({ page }) => {
      await parkAboveTheEnd(page, PARK_PX);
      if (reader === "at the end") {
        await readerScrollsToEnd(page);
        await afterLayoutSettles(page);
      }
      const saved = await visibleRow(page);
      let result: {
        before: number;
        after: number;
        emptyRows: number;
        clientHeight: number;
        range: number;
        collapsedTop: number;
        clamp: number;
        events: string[];
        detachedMutations: number | null;
      };
      try {
        result = await page.evaluate(() => {
          const c = document.getElementById("chat-scroll-container")!;
          const content = document.getElementById("chat-content")!;
          const events: string[] = [];
          const onScroll = () => events.push("scroll");
          const onScrollEnd = () => events.push("scrollend");
          const observers = [content, c].map((target, index) => {
            const observer = new ResizeObserver(() => events.push(index === 0 ? "content-resize" : "container-resize"));
            observer.observe(target);
            return observer;
          });
          const beforeTop = c.scrollTop;
          c.addEventListener("scroll", onScroll);
          c.addEventListener("scrollend", onScrollEnd);
          let detachedMutations: number | null = null;
          try {
            const { before, after } = window.__riverEmptyRender!.empty(false);
            const emptyRows = document.querySelectorAll("#chat-content .anchor-row").length;
            // Force both the content range and native scroll clamp before the
            // real controller consumes the undelivered layout movement.
            const clientHeight = c.clientHeight;
            const range = c.scrollHeight - c.clientHeight;
            const collapsedTop = c.scrollTop;
            const clamp = beforeTop - collapsedTop;
            window.__riverTest!.takeInPendingHistoryScroll();
            detachedMutations = window.__riverEmptyRender!.restore();
            void document.getElementById("chat-content")!.getBoundingClientRect().height;
            window.__riverTest!.restoreHistoryPosition();
            return { before, after, emptyRows, clientHeight, range, collapsedTop, clamp, events, detachedMutations };
          } finally {
            // The retained Dioxus nodes must be back before this evaluation
            // yields, including when an assertion inside the fixture fails.
            detachedMutations ??= window.__riverEmptyRender!.restore();
            c.removeEventListener("scroll", onScroll);
            c.removeEventListener("scrollend", onScrollEnd);
            for (const observer of observers) observer.disconnect();
          }
        });
      } finally {
        await restoreHistory(page);
      }
      expect(result.emptyRows, "premise: the history collapsed to no message rows").toBe(0);
      expect(result.clientHeight, "premise: the scroll container remains visible and laid out").toBeGreaterThan(0);
      expect(result.range, "premise: the empty range is shorter than the viewport").toBeLessThanOrEqual(1);
      expect(result.collapsedTop, "premise: collapse clamped scrollTop to the top").toBeLessThanOrEqual(1);
      expect(result.clamp, "premise: the layout clamp is well beyond the 200px allowance").toBeGreaterThan(200);
      expect(result.events, "no browser scroll or resize event was delivered before refill").toEqual([]);
      expect(result.detachedMutations, "the app did not mutate retained nodes while detached").toBe(0);
      await expectVisibleRowHolds(page, saved, "the synchronous refill lost the saved row");
    });
  }

  test("capture: a reader scroll while no rows render keeps the saved message for when they return", async ({ page }) => {
    await parkAboveTheEnd(page, PARK_PX);
    const saved = await visibleRow(page);
    try {
      const { before, after } = await emptyHistory(page, { keepHeight: true });
      expect(Math.abs(after - before), "premise: the placeholder keeps the content's height").toBeLessThanOrEqual(1);
      await expectEmptyButVisible(page);
      await afterObserverAndTask(page);
      const move = await page.evaluate(
        () =>
          new Promise<{ before: number; after: number; delivered: boolean }>((resolve) => {
            const c = document.getElementById("chat-scroll-container")!;
            const before = c.scrollTop;
            const done = (delivered: boolean) => resolve({ before, after: c.scrollTop, delivered });
            const timer = setTimeout(() => done(false), 2_000);
            c.addEventListener("scroll", () => (clearTimeout(timer), done(true)), { once: true });
            c.scrollTop = before - 600;
          }),
      );
      expect(move.delivered, `premise: the reader's scroll reached the app (${JSON.stringify(move)})`).toBe(true);
      expect(move.before - move.after, "premise: the reader's scroll moved the view").toBeGreaterThan(
        LAYOUT_SHIFT_ALLOWANCE_PX,
      );
      await afterLayoutSettles(page);
      await expectEmptyButVisible(page);
    } finally {
      await restoreHistory(page);
    }
    await afterLayoutSettles(page);
    await deliver(page, ARRIVAL("arrival after the rows came back"));
    await expectVisibleRowHolds(page, saved!, "the saved message was lost to the empty render");
    expect(await distanceFromBottom(page), "the reader was sent to the latest message").toBeGreaterThan(
      PARKED_ABOVE_END_PX,
    );
  });

  test("navigation: rows disappearing mid-flight stop it, and the last captured message comes back", async ({ page }) => {
    await parkAboveTheEnd(page, PARK_PX);
    await recordScrollRequests(page);
    const parkedAt = await scrollTop(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    let captured: RowPosition | null = null;
    try {
      const emptied = await page.evaluate(
        () =>
          new Promise<{ anchor: RowPosition | null; before: number; after: number }>((resolve) => {
            const c = document.getElementById("chat-scroll-container")!;
            c.addEventListener(
              "scroll",
              () => {
                const anchor = window.__riverHistoryGeometry!.newestVisible(c);
                resolve({ anchor, ...window.__riverEmptyRender!.empty(true) });
              },
              { once: true },
            );
          }),
      );
      expect(emptied.anchor, "premise: a message was visible at the navigation's scroll").not.toBeNull();
      expect(Math.abs(emptied.after - emptied.before), "premise: the placeholder keeps the height").toBeLessThanOrEqual(1);
      captured = emptied.anchor;
      await expectEmptyButVisible(page);
      await viewAtRest(page, "the navigation should stop once its rows are gone");
      expect(await distanceFromBottom(page), "the navigation went on to the end with no rows").toBeGreaterThan(200);
      await expectEmptyButVisible(page);
    } finally {
      await restoreHistory(page);
    }
    await afterLayoutSettles(page);
    await deliver(page, ARRIVAL("arrival after the rows came back"));
    await expectVisibleRowHolds(page, captured!, "the navigation's last captured message was lost to the empty render");
    expect(await scrollRequests(page), "nothing re-issued the animation").toEqual({ smooth: 1, other: 0 });
  });

  test("navigation: rows gone at its native end leave the last captured message for when they return", async ({
    page,
  }) => {
    await parkAboveTheEnd(page, PARK_PX);
    await recordScrollRequests(page);
    const parkedAt = await scrollTop(page);
    const destination = await maxScrollTop(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    let captured: RowPosition | null = null;
    try {
      const emptied = await page.evaluate(
        () =>
          new Promise<{ anchor: RowPosition | null; top: number; before: number; after: number }>((resolve) => {
            const c = document.getElementById("chat-scroll-container")!;
            const onEnd = (e: Event) => {
              if (e.target !== c) return;
              window.removeEventListener("scrollend", onEnd, true);
              const top = c.scrollTop;
              const anchor = window.__riverHistoryGeometry!.newestVisible(c);
              resolve({ anchor, top, ...window.__riverEmptyRender!.empty(true) });
            };
            window.addEventListener("scrollend", onEnd, true);
          }),
      );
      expect(emptied.anchor, "premise: a message was visible at the end").not.toBeNull();
      expect(
        Math.abs(emptied.top - destination),
        "premise: the end came at the destination, so it can finish the navigation",
      ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
      expect(Math.abs(emptied.after - emptied.before), "premise: the placeholder keeps the height").toBeLessThanOrEqual(1);
      captured = emptied.anchor;
      await afterObserverAndTask(page);
      await expectEmptyButVisible(page);
    } finally {
      await restoreHistory(page);
    }
    await afterLayoutSettles(page);
    await deliver(page, ARRIVAL("arrival after the rows came back"));
    await expectVisibleRowHolds(page, captured!, "the landing was lost to the empty render at the navigation's end");
    expect(await distanceFromBottom(page), "the arrival was followed").toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
    expect(await scrollRequests(page), "nothing re-issued the animation").toEqual({ smooth: 1, other: 0 });
  });

  test("a click on scroll-to-latest while no rows render starts nothing", async ({ page }) => {
    await parkAboveTheEnd(page, PARK_PX);
    const saved = await visibleRow(page);
    await recordScrollRequests(page);
    try {
      await emptyHistory(page, { keepHeight: true });
      await afterObserverAndTask(page);
      await expectEmptyButVisible(page);
      await expect(button(page), "premise: the button is still offered").toBeVisible();
      const top = await scrollTop(page);
      await button(page).click();
      await afterLayoutSettles(page);
      expect(await scrollRequests(page), "a click with no rows asked for an animation").toEqual({ smooth: 0, other: 0 });
      expect(await scrollTop(page), "a click with no rows moved the view").toBeCloseTo(top, 0);
    } finally {
      await restoreHistory(page);
    }
    await afterLayoutSettles(page);
    await deliver(page, ARRIVAL("arrival after the rows came back"));
    await expectVisibleRowHolds(page, saved!, "the saved message was lost to the click with no rows");
  });
});
