import { test, expect, Page, Route } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { clampCleanup, clampOverhang, expectFinalEndClamp } from "./history-clamp-fixture";
import {
  newestVisibleRow,
  registerHistoryGeometry,
  savedVisibleRowDrift,
  type RowPosition,
} from "./history-scroll-geometry";
import {
  orderGrowAboveThenEnd,
  orderLog,
  orderRealFrames,
  orderReaderMove,
  orderRecorderStart,
  orderRecorderStop,
} from "./history-event-order-fixture";
import {
  AT_BOTTOM_EPSILON_PX,
  IN_PLACE_TOLERANCE_PX,
  PARKED_ABOVE_END_PX,
  TALL,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  expectDriftWithin,
  expectSettledAtBottom,
  fillHistory,
  openRoomAtBottom,
  readerScrollsToEnd,
  readerScrollsWithoutGesture,
  scrollTop,
  viewportHeight,
} from "./history-scroll-helpers";

// The reader's saved row across layout and native-event orderings that a
// geometry heuristic could get wrong (history_scroll.rs, "Classifying a
// `scroll` event" and "Timing and visibility"): a clamp to a shorter range, a
// clamp while hidden, the one reading anchor deleted, and a reflow above the reader
// that the ResizeObserver reports after some other event.
//
// Arrivals never move the view, so every case asks the same thing: is the
// reader's row where they left it. Deleting that one row places at the latest
// message once, and a stray `scrollend` or a reveal cannot revive the old
// offset or start following. `scrollend` finishes only a scroll-to-latest
// navigation now; the synthetic ones below stand in for an engine's end
// arriving early or late, and must change nothing.
//
// Assumes the example-data build (`window.__riverTest`). Arrivals are INBOUND.

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
});

/// Deliver an inbound message and wait until its row is on the page and the
/// layout it caused has reached the app. The row may be below the view.
async function arrive(page: Page, text: string) {
  await callRiverTest(page, "appendMessage", text);
  await expect(
    page.getByText(text.split("\n")[0].slice(0, 40), { exact: false }).last(),
    "premise: the arrival was delivered",
  ).toBeAttached({ timeout: 5_000 });
  await afterLayoutSettles(page);
}

/// The saved row is back at its gap, in view, and stays there for five samples
/// over 500ms.
function expectRowHeld(page: Page, before: RowPosition, why: string) {
  return expectDriftWithin(page, () => savedVisibleRowDrift(page, before), why, { hold: true });
}

// Removing a positioned overhang shortens the scroll range without resizing
// anything the layout signature records, and the browser clamps the parked
// reader to the new end. Their saved gap is out of reach for now; the restore
// must not take the clamp for their choice, so a later arrival that makes the
// range long enough again brings their row back to its gap. Which of the
// clamp's `scroll` and a stray `scrollend` reaches the app first must not
// matter.
test.describe("A clamp to a shorter range", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  for (const endFirst of [false, true]) {
    test(`a signature-unchanged clamp keeps the parked reader's gap for a later arrival to restore (controlled order: ${endFirst ? "clamp → end → scroll" : "clamp → scroll → end"})`, async ({
      page,
    }) => {
      await openRoomAtBottom(page, "Team Chat Room");
      await fillHistory(page);
      const overhang = await clampOverhang(page);
      try {
        await readerScrollsWithoutGesture(page, overhang.after.max);
        await afterLayoutSettles(page);
        await orderRecorderStart(page);
        const move = await orderReaderMove(page, -400);
        expect(move.delivered, "premise: the reader's move reached the app").toBe(true);
        expect(move.after - move.before, "premise: the reader moved up into the overhang").toBeCloseTo(-400, 0);
        // Let the move's own native end, where the engine sends one, go by.
        await orderRealFrames(page, 3);
        expect(await distanceFromBottom(page), "premise: the reader is parked well above the end").toBeGreaterThan(
          PARKED_ABOVE_END_PX,
        );
        const at = await newestVisibleRow(page);
        expect(at, "premise: a message is visible").not.toBeNull();

        const clamped = await page.evaluate(async (endFirst) => {
          const rec = window.__historyOrder!;
          rec.log.push("clamp");
          const removed = window.__historyClamp.remove();
          // Let the real clamp scroll reach the app first, or dispatch the end
          // in the same task, before it can.
          if (!endFirst) for (let i = 0; i < 3; i++) await new Promise<void>((r) => rec.afterRealFrame(r));
          const scrollsBeforeEnd = window.__historyClamp.scrolls;
          document.getElementById("chat-scroll-container")!.dispatchEvent(new Event("scrollend"));
          return { removed, scrollsBeforeEnd, log: rec.log.slice(rec.log.lastIndexOf("clamp")).join(" ") };
        }, endFirst);
        expectFinalEndClamp(clamped.removed);
        if (endFirst) {
          expect(clamped.scrollsBeforeEnd, "premise: the end precedes the clamp's scroll").toBe(clamped.removed.scrolls);
          expect(clamped.log, "premise: the end reached the app right after the clamp").toBe("clamp end");
        } else {
          expect(clamped.scrollsBeforeEnd, "premise: the clamp's scroll arrived before the end").toBeGreaterThan(
            clamped.removed.scrolls,
          );
          expect(clamped.log, "premise: the clamp's scroll reached the app before the end").toMatch(/^clamp scroll\b.* end$/);
        }
        await orderRealFrames(page, 3);
        expect(
          await page.evaluate(() => window.__historyClamp.scrolls),
          "premise: the clamp's scroll was delivered",
        ).toBeGreaterThan(clamped.removed.scrolls);
        expect(await scrollTop(page), "the unreachable gap leaves the view at the clamped end").toBeCloseTo(
          clamped.removed.after.top,
          0,
        );

        const marker = "arrival after the clamp";
        await arrive(page, TALL(marker, 30));
        const height = await page.getByText(marker, { exact: false }).last().evaluate((el) => el.getBoundingClientRect().height);
        expect(height, "premise: the arrival gives back more than the clamp took").toBeGreaterThan(500);
        await expectRowHeld(
          page,
          at!,
          `the clamp was taken for the reader's position, so the arrival did not bring their row back (${await orderLog(page)})`,
        );
        expect(await distanceFromBottom(page), "the arrival should be below the view").toBeGreaterThan(PARKED_ABOVE_END_PX);
      } finally {
        await orderRecorderStop(page);
        await clampCleanup(page);
      }
    });
  }
});

// The same clamp, made while the mobile layout hides the chat: nothing can be
// measured then, and the reveal finds the range shorter than the saved gap
// needs. The reveal restores rather than capturing, so the gap stays saved for
// the later geometry that can reach it (plan decision 8).
test.describe("An unreachable saved gap across a hide and reveal", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("a clamp while the chat is hidden leaves the reveal at the clamped end, and a later arrival brings the reader's row back", async ({
    page,
  }) => {
    const chat = page.locator("#chat-scroll-container");
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    const overhang = await clampOverhang(page);
    try {
      await readerScrollsWithoutGesture(page, overhang.after.max);
      await afterLayoutSettles(page);
      await readerScrollsWithoutGesture(page, (await scrollTop(page)) - 400);
      await afterLayoutSettles(page);
      expect(await distanceFromBottom(page), "premise: the reader is parked well above the end").toBeGreaterThan(
        PARKED_ABOVE_END_PX,
      );
      const at = await newestVisibleRow(page);
      expect(at, "premise: the saved row is visible before hiding").not.toBeNull();
      const before = await page.evaluate(() => window.__historyClamp.snapshot());

      await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
      await expect(chat).toBeHidden({ timeout: 5_000 });
      await expect.poll(() => viewportHeight(page), { message: "premise: hidden geometry is not measured" }).toBe(0);
      await afterLayoutSettles(page);
      await page.evaluate(() => window.__historyClamp.remove());
      await page.getByTestId("rooms-back-button").click();
      await expect(chat).toBeVisible();
      await expect.poll(() => viewportHeight(page), { message: "premise: the back button reveals the history" }).toBeGreaterThan(0);
      await afterLayoutSettles(page);
      const after = await page.evaluate(() => window.__historyClamp.snapshot());
      expectFinalEndClamp({ before, after, scrolls: 0 });

      const marker = "tall arrival after the hidden clamp";
      await arrive(page, TALL(marker, 30));
      expect(
        await page.getByText(marker, { exact: false }).last().evaluate((el) => el.getBoundingClientRect().height),
        "premise: the arrival gives back more than the clamp took",
      ).toBeGreaterThan(500);
      await expectRowHeld(page, at!, "the reveal took the clamped end for the reader's position, so the arrival did not bring their row back");
      expect(await distanceFromBottom(page), "the arrival should be below the view").toBeGreaterThan(PARKED_ABOVE_END_PX);
    } finally {
      await clampCleanup(page);
    }
  });
});

// The one saved anchor is deleted. While the history is measurable that places
// at the latest message once; a stray `scrollend` must not revive the old
// offset or start following. While the history is hidden, zero measurements
// are not that deletion: the latest placement waits until the chat has a box
// again. The landing is then preserved through later arrivals.

/// Enough filler that the deleted row is a plain message with a neighbor above it.
const MISSING_FILLERS = 24;

/// Park on a filler, record the row above it, and delete only the parked row.
async function deleteParkedAnchor(page: Page) {
  await openRoomAtBottom(page, "Team Chat Room");
  await fillHistory(page, MISSING_FILLERS);
  await readerScrollsWithoutGesture(page, (await scrollTop(page)) - 300);
  await afterLayoutSettles(page);
  const anchor = await newestVisibleRow(page);
  expect(anchor, "premise: a message is visible").not.toBeNull();
  const neighborAlive = await page.evaluate((id) => {
    const rows = Array.from(document.querySelectorAll<HTMLElement>("#chat-content [data-anchor-row]"));
    const at = rows.findIndex((row) => row.id === id);
    const neighbor = at > 0 ? rows[at - 1] : null;
    return neighbor?.id.startsWith("msg-") ?? false;
  }, anchor!.id);
  expect(neighborAlive, "premise: a message survives above the anchor").toBe(true);
  await orderRecorderStart(page);
  const unmatched = await callRiverTest(page, "removeMessages", [anchor!.id]);
  expect(unmatched, "premise: the anchor id named a message").toEqual([]);
  await expect
    .poll(() => page.evaluate((id) => document.getElementById(id) === null, anchor!.id), { timeout: 5_000 })
    .toBe(true);
  return anchor!.id;
}

/// Two tall arrivals must leave the landing row in place, below them.
async function expectLandingPreserved(page: Page, landed: RowPosition, why: string) {
  for (const n of [1, 2]) {
    const marker = `tall arrival ${n} after the anchor was deleted`;
    await arrive(page, TALL(marker, 30));
    const height = await page
      .getByText(marker, { exact: false })
      .last()
      .evaluate((el) => el.getBoundingClientRect().height);
    expect(height, "premise: the arrival is far taller than the tolerance").toBeGreaterThan(
      PARKED_ABOVE_END_PX + IN_PLACE_TOLERANCE_PX,
    );
    await expectRowHeld(page, landed, `${why}: arrival ${n} moved the landing (${await orderLog(page)})`);
    expect(await distanceFromBottom(page), `${why}: arrival ${n} should be below the view`).toBeGreaterThan(
      PARKED_ABOVE_END_PX,
    );
  }
}

test.describe("The reading anchor is deleted", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a stray scrollend cannot revive the old offset, and later arrivals do not follow", async ({ page }) => {
    try {
      await deleteParkedAnchor(page);
      await expectSettledAtBottom(page, "deleting the reading anchor should place at the latest message");
      const landed = await newestVisibleRow(page);
      expect(landed, "premise: the landing has a visible message").not.toBeNull();
      const ended = await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        const top = c.scrollTop;
        c.dispatchEvent(new Event("scrollend"));
        return { top, after: c.scrollTop };
      });
      expect(ended.after, `the stray end moved the view (${await orderLog(page)})`).toBe(ended.top);
      await orderRealFrames(page, 2);
      await expectRowHeld(page, landed!, "a stray scrollend moved the landing");
      await expectLandingPreserved(page, landed!, "a stale event re-enabled following");
    } finally {
      await orderRecorderStop(page);
    }
  });
});

test.describe("The reading anchor is deleted while the chat is hidden", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("the reveal places at the latest message once the chat is measurable, and the next arrival is preserved", async ({
    page,
  }) => {
    const chat = page.locator("#chat-scroll-container");
    try {
      await openRoomAtBottom(page, "Team Chat Room");
      await fillHistory(page, MISSING_FILLERS);
      await readerScrollsWithoutGesture(page, (await scrollTop(page)) - 300);
      await afterLayoutSettles(page);
      const anchor = await newestVisibleRow(page);
      expect(anchor, "premise: a message is visible").not.toBeNull();
      await orderRecorderStart(page);

      await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
      await expect(chat).toBeHidden({ timeout: 5_000 });
      await expect.poll(() => viewportHeight(page), { message: "premise: hidden geometry is not measured" }).toBe(0);
      const unmatched = await callRiverTest(page, "removeMessages", [anchor!.id]);
      expect(unmatched, "premise: the anchor id named a message").toEqual([]);
      await orderRealFrames(page, 2);

      await page.getByTestId("rooms-back-button").click();
      await expect(chat).toBeVisible();
      await expect.poll(() => viewportHeight(page), { message: "premise: the back button reveals the history" }).toBeGreaterThan(0);
      await expectSettledAtBottom(page, "the reveal should place at the latest message once the chat is measurable");
      const landed = await newestVisibleRow(page);
      expect(landed, "premise: the reveal has a visible message").not.toBeNull();
      expect(landed!.id, "the deleted anchor must not be the landing").not.toBe(anchor!.id);
      await expectLandingPreserved(page, landed!, "the reveal landing was not preserved");
    } finally {
      await orderRecorderStop(page);
    }
  });
});

// Content can grow above the reader with `scrollTop` unchanged (an image
// loading; the container sets `overflow-anchor: none`), so no `scroll` event is
// coming, only the ResizeObserver's report, and the restore it triggers puts
// the saved row back. Whatever reaches the app before that report (here a
// `scrollend`, as an engine ending the reader's wheel can send in the same
// rendering pass) must not save the reflowed rows as the reader's position.
//
// The order is made, not waited for: the growth and a synthetic `scrollend` in
// one task, so the end runs before any observer can deliver. The probe below
// asks what engines produce natively.
test.describe("A reflow above the reader, reported after another event", () => {
  test.use({ viewport: { width: 1280, height: 900 } });
  /// Growth above the reader's row.
  const GROW_PX = 300;

  /// Move `upPx` up from the end, then the growth and an end in one task.
  /// Checks the order and that the growth moved the reader's message before the
  /// app saw anything. Leaves the recorder running.
  async function reflowBeforeObserver(page: Page, upPx: number) {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await orderRecorderStart(page);
    const move = await orderReaderMove(page, -upPx);
    expect(move.delivered, "premise: the reader's scroll event reached the app").toBe(true);
    expect(move.after - move.before, "premise: the reader moved up").toBeCloseTo(-upPx, 0);
    // Let the move's own native end, where the engine sends one, go by.
    await orderRealFrames(page, 3);
    const at = await newestVisibleRow(page);
    expect(at, "premise: a message is visible").not.toBeNull();
    const run = await orderGrowAboveThenEnd(page, GROW_PX, at!.id);
    const log = await orderLog(page);
    const what =
      `${at!.id} at ${at!.gap.toFixed(1)}px, moved ${run.shift.toFixed(1)}px by the growth; ` +
      `scrollTop ${run.before} → ${run.grown} grown → ${run.ended} after the end; ${log}`;
    expect(log.slice(log.lastIndexOf("grow")), `premise: the end reached the app before the observer (${what})`).toMatch(
      /^grow end\b.*\bobserved\b/,
    );
    expect(Math.abs(run.grown - run.before), `premise: the growth did not move the view (${what})`).toBeLessThanOrEqual(1);
    expect(
      Math.abs(run.shift - GROW_PX),
      `premise: the growth pushed the reader's message down before the app saw it (${what})`,
    ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
    return { at: at!, what };
  }

  test("a reader parked well above the end keeps their message (controlled order: growth → end → observer)", async ({
    page,
  }) => {
    try {
      const { at, what } = await reflowBeforeObserver(page, 200);
      await orderRealFrames(page, 3);
      expect(await savedVisibleRowDrift(page, at), `the reader's message moved with the reflow (${what})`).toBeLessThanOrEqual(
        IN_PLACE_TOLERANCE_PX,
      );
      expect(await distanceFromBottom(page), `premise: the reader is parked well above the end (${what})`).toBeGreaterThan(
        PARKED_ABOVE_END_PX,
      );
      await deliver(page, `arrival after the reflow: ${"r".repeat(200)}`);
      await expectRowHeld(page, at, `an arrival after the reflow moved the reader's message (${await orderLog(page)})`);
    } finally {
      await orderRecorderStop(page);
    }
  });

  test("a reader just above the end keeps their message too, and the next arrival is not followed (controlled order: growth → end → observer)", async ({
    page,
  }) => {
    try {
      const { at, what } = await reflowBeforeObserver(page, 20);
      await orderRealFrames(page, 3);
      expect(await savedVisibleRowDrift(page, at), `the reader's message moved with the reflow (${what})`).toBeLessThanOrEqual(
        IN_PLACE_TOLERANCE_PX,
      );
      await deliver(page, `arrival after the reflow: ${"r".repeat(200)}`);
      await expectRowHeld(page, at, `an arrival after the reflow moved the reader's message (${await orderLog(page)})`);
      expect(await distanceFromBottom(page), "the arrival should be below the view, not followed").toBeGreaterThan(
        AT_BOTTOM_EPSILON_PX,
      );
    } finally {
      await orderRecorderStop(page);
    }
  });

  // Native, on the browser's own clock: three wheel ticks up, then a real image
  // above the reader finishes loading, its response sent each delay in
  // PROBE_DELAYS_MS after the last tick. The page records every `scroll` and
  // `scrollend` once the app has had it, the image's `load`, and every
  // observer delivery, each with the image row's height. The order of interest
  // is an end that first sees the grown row before any observer has reported
  // it. Whatever order the engine chose, the reader's message has to stay where
  // their last scroll left it. Ordering evidence only: it says nothing about
  // real wheel lifetime.
  //
  // Measured on the old controller (2026-10-02, headless): Chromium ends every
  // wheel tick at once, and Firefox sent no end of its own before the
  // correction's; WebKit ends the wheel ~100ms after its last scroll, in the
  // same rendering pass as the observer and before it, so an image loading
  // 75-85ms after the ticks produced the order natively. The delays straddle
  // that window; elsewhere they only check the outcome.
  test("native input probe: wheel ticks, then an image loading above the reader", async ({ page, isMobile }) => {
    test.skip(
      isMobile,
      "no wheel input on the mobile projects: page.mouse.wheel is unsupported on mobile WebKit, and mobile Chromium is a touch device",
    );
    test.setTimeout(120_000);
    const PROBE_DELAYS_MS = [25, 75, 80, 85, 110];
    const WHEEL_PX = 60;
    for (const [i, delay] of PROBE_DELAYS_MS.entries()) {
      const url = `/late-probe-${i}.svg`;
      let held: Route | null = null;
      await page.route(`**${url}`, (route) => {
        held = route;
      });
      try {
        await openRoomAtBottom(page, "Team Chat Room");
        await callRiverTest(page, "appendMessage", `late probe ![late](${url})`);
        await expect.poll(() => held !== null, { message: "premise: the image was requested" }).toBe(true);
        for (let f = 0; f < 16; f++) await deliver(page, `probe filler ${f}: ${"y".repeat(200)}`);
        await readerScrollsToEnd(page);
        await afterLayoutSettles(page);
        const box = (await page.locator("#chat-scroll-container").boundingBox())!;
        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await page.evaluate((url) => {
          const c = document.getElementById("chat-scroll-container")!;
          const img = c.querySelector<HTMLImageElement>(`img[src$="${url}"]`)!;
          const row = img.closest<HTMLElement>('[id^="msg-"]')!;
          const events: ProbeEvent[] = [];
          const newest = () => {
            const geo = window.__riverHistoryGeometry;
            if (!geo) throw new Error("history geometry is not installed");
            return geo.newestVisible(c);
          };
          const push = (kind: string) =>
            events.push({
              kind,
              t: performance.now(),
              top: c.scrollTop,
              rowHeight: row.getBoundingClientRect().height,
              rowBottom: row.getBoundingClientRect().bottom - c.getBoundingClientRect().top,
              at: newest(),
            });
          // At the target, after the app's own listeners.
          c.addEventListener("scroll", () => push("scroll"));
          c.addEventListener("scrollend", () => push("end"));
          img.addEventListener("load", () => push("load"));
          new ResizeObserver(() => push("observed")).observe(document.getElementById("chat-content")!);
          (window as unknown as { __lateProbe: ProbeEvent[] }).__lateProbe = events;
        }, url);
        for (let tick = 0; tick < 3; tick++) {
          await page.mouse.wheel(0, -WHEEL_PX);
          await page.waitForTimeout(20);
        }
        await page.waitForTimeout(delay);
        await held!.fulfill({
          contentType: "image/svg+xml",
          body: '<svg xmlns="http://www.w3.org/2000/svg" width="320" height="240"><rect width="320" height="240" fill="#888"/></svg>',
        });
        held = null;
        await expect
          .poll(() => page.evaluate(() => (window as unknown as { __lateProbe: ProbeEvent[] }).__lateProbe.some((e) => e.kind === "load")), {
            message: "premise: the image loaded",
          })
          .toBe(true);
        await afterLayoutSettles(page);
        await afterLayoutSettles(page);
        const events = await page.evaluate(() => (window as unknown as { __lateProbe: ProbeEvent[] }).__lateProbe);
        const final = await newestVisibleRow(page);

        const pendingHeight = events[0].rowHeight;
        const grownAt = events.findIndex((e) => e.rowHeight > pendingHeight + 1);
        expect(grownAt, "premise: the image made its row taller").toBeGreaterThan(-1);
        const growth = events[grownAt].rowHeight - pendingHeight;
        const before = events.slice(0, grownAt);
        const lastScroll = before.map((e) => e.kind).lastIndexOf("scroll");
        expect(lastScroll, "premise: the wheel scrolled before the image loaded").toBeGreaterThan(-1);
        const base = before[lastScroll];
        expect(base.rowBottom, "premise: the image's row was above the view").toBeLessThan(0);
        const t0 = base.t;
        const timeline = events
          .map((e, n) => `${e.kind}${n === grownAt ? "*" : ""}@${Math.round(e.t - t0)}`)
          .join(" ");
        const endedBefore = before.slice(lastScroll).some((e) => e.kind === "end");
        const afterGrowth = events.slice(grownAt);
        const firstObserved = afterGrowth.findIndex((e) => e.kind === "observed");
        const endFirst =
          !endedBefore && afterGrowth.slice(0, firstObserved < 0 ? undefined : firstObserved).some((e) => e.kind === "end");
        // A scroll after the growth that is neither the growth's (no move) nor
        // its correction is the reader still moving.
        const readerAfter = afterGrowth.some(
          (e) =>
            e.kind === "scroll" &&
            Math.abs(e.top - events[grownAt].top) > 1 &&
            Math.abs(e.top - events[grownAt].top - growth) > 1,
        );
        const drift = final?.id === base.at?.id ? Math.abs(final!.gap - base.at!.gap) : Infinity;
        const description =
          `delay ${delay}ms: ${timeline} (* first sight of the ${growth.toFixed(0)}px growth); ` +
          `end before observer: ${endFirst ? "OBSERVED" : "not observed"}${endedBefore ? " (ended before the growth)" : ""}; ` +
          `reader moved after the growth: ${readerAfter}; drift ${drift.toFixed(1)}px`;
        test.info().annotations.push({ type: "native late image", description });
        if (!readerAfter) {
          expect(drift, `the reader's message moved (${description})`).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
        }
      } finally {
        await (held as Route | null)?.abort().catch(() => {});
        await page.unroute(`**${url}`);
      }
    }
  });
});

/// One line of the late-image probe's record.
type ProbeEvent = {
  kind: string;
  t: number;
  top: number;
  rowHeight: number;
  /// The image row's bottom edge, from the container's top edge.
  rowBottom: number;
  at: { id: string; gap: number } | null;
};
