import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import {
  newestVisibleRow,
  registerHistoryGeometry,
  savedRowDrift,
  type RowPosition,
} from "./history-scroll-geometry";
import {
  endReflowAtNextScrollend,
  endReflowResult,
  endReflowUnhide,
  orderLog,
  orderRecorderStart,
  orderRecorderStop,
  recordMidflightArrival,
  registerEndReflow,
  removeSyntheticArrivals,
  type EndReflow,
  type EndReflowKind,
  type MidflightArrival,
} from "./history-event-order-fixture";
import {
  ARRIVAL,
  AT_BOTTOM_EPSILON_PX,
  IN_PLACE_TOLERANCE_PX,
  animationUnderway,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  expectDriftWithin,
  expectVisibleRowHolds,
  expectSettledAtBottom,
  hideChat,
  maxScrollTop,
  openRoomAtBottom,
  parkAboveTheEnd,
  readerScrollsWithoutGesture,
  recordScrollRequests,
  revealChat,
  scrollRequests,
  scrollTop,
  viewAtRest,
} from "./history-scroll-helpers";

// "Scroll to latest messages" is ONE native smooth scroll to the end measured at
// the click (ui/src/components/conversation/history_scroll.rs). The browser owns
// its duration and easing; these tests run on the browser's real clock and make
// no claim about either. What they hold it to:
//
//   * one `scrollTo({behavior: "smooth"})` per click, never re-aimed, never
//     followed by a corrective jump to an end that moved on;
//   * where it lands (or where the reader's input, a hide, or a second click
//     stops it) is preserved through later arrivals like any other position;
//   * a hide cancels it for good: the reveal restores, it does not resume.
//
// Arrivals are INBOUND (`window.__riverTest`, example-data build).

/// How far above the end the reader parks before the click.
const PARK_PX = 3_000;
/// The newest messages the mid-flight deletion leaves for the landing.
const KEEP_LATEST = 2;
const GROW_PX = 200;
const MAX_TRIES = 8;

const button = (page: Page) => page.getByTestId("scroll-to-bottom");

/// Park well above the end with the button showing, then start recording.
async function parkAndRecord(page: Page) {
  await parkAboveTheEnd(page, PARK_PX);
  await recordScrollRequests(page);
  return { parkedAt: await scrollTop(page), destination: await maxScrollTop(page) };
}

/// The row recorded in `before` is still at its gap, over five samples (500ms).
function expectRowHolds(page: Page, before: RowPosition, why: string) {
  return expectDriftWithin(page, () => savedRowDrift(page, before), why, { hold: true });
}

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
  await registerEndReflow(page);
});

test.describe("Scroll to latest is one native smooth scroll", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("with no arrivals it reaches the end, and the landing is preserved through the next arrival", async ({
    page,
  }) => {
    await parkAndRecord(page);
    await button(page).click();
    await expectSettledAtBottom(page, "the native animation did not reach the end");
    await viewAtRest(page, "the view should come to rest at the end");
    expect(await scrollRequests(page), "one smooth request for one click").toEqual({ smooth: 1, other: 0 });

    const landed = await newestVisibleRow(page);
    expect(landed, "premise: a message should be visible").not.toBeNull();
    await deliver(page, ARRIVAL("arrival after the landing"));
    await expectRowHolds(page, landed!, "an arrival after the landing moved the view");
    expect(await distanceFromBottom(page), "the arrival should be below the view, not followed").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );
  });

  /// Where arrivals asked for at the navigation's first `scroll` must leave it:
  /// at the click's destination, one request, the arrivals below the view, and
  /// nothing moving that landing afterwards.
  async function expectArrivalsNotFollowed(page: Page, proof: MidflightArrival, destination: number) {
    const what = JSON.stringify(proof);
    expect(proof.destination, `premise: the probe clicked at the parked end (${what})`).toBeCloseTo(destination, 0);
    const rest = await viewAtRest(page, "the view should come to rest");
    expect(await scrollRequests(page), "the click's request is the only one").toEqual({ smooth: 1, other: 0 });
    expect(Math.abs(rest - destination), `it should land at the end measured at the click (${what})`).toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    expect(await distanceFromBottom(page), "the newer arrivals stay below the view").toBeGreaterThan(200);
    await expect(button(page), "the button stays offered for the newer messages").toBeVisible();

    // Nothing comes back for it later: no completion snap, no quiet-interval jump.
    const landed = await newestVisibleRow(page);
    expect(landed, "premise: a message should be visible").not.toBeNull();
    await page.waitForTimeout(1_500);
    await expectRowHolds(page, landed!, "the landing moved after the animation ended");
    await deliver(page, ARRIVAL("arrival after the landing"));
    await expectRowHolds(page, landed!, "an arrival after the landing moved the view");
  }

  // The real arrival path: state change, render, ResizeObserver. The hook
  // defers, and a fast engine (CI's Linux WebKit moves ~2,000px a frame) can
  // land before the render, so this case reports whether the rows attached
  // mid-flight and checks the outcome either way. The synchronous case below
  // is the one that proves mid-flight attachment on every engine.
  test("arrivals asked for during the animation do not retarget it or snap it to the new end", async ({ page }) => {
    const { destination } = await parkAndRecord(page);
    const arrivals = [0, 1, 2].map((i) => ARRIVAL(`arrival ${i} mid-flight`));
    const proof = await recordMidflightArrival(page, arrivals);
    expect(proof.attached).toEqual(arrivals);
    test.info().annotations.push({
      type: "arrival attachment",
      description: `${proof.midflightAtAttachment ? "mid-flight (proven)" : "late, not counted as mid-flight"}; ${
        proof.timeline
      }`,
    });
    await expectArrivalsNotFollowed(page, proof, destination);
  });

  test("rows attached while the animation runs do not retarget it or snap it to the new end", async ({ page }) => {
    const { destination } = await parkAndRecord(page);
    try {
      const arrivals = [0, 1, 2].map((i) => `synthetic arrival ${i}`);
      const proof = await recordMidflightArrival(page, arrivals, { attach: "synchronous" });
      expect(proof.attached).toEqual(arrivals);
      expect(proof.attachedBeforeEnd, `premise: the rows attached before the navigation ended (${proof.timeline})`).toBe(
        true,
      );
      expect(
        proof.progressedAfterAttachment,
        `premise: the animation moved on with the rows attached (${proof.timeline})`,
      ).toBe(true);
      await expectArrivalsNotFollowed(page, proof, destination);
    } finally {
      await removeSyntheticArrivals(page);
    }
  });

  test("the attachment-time probe rejects arrivals requested after the landing", async ({ page }) => {
    await parkAndRecord(page);
    const proof = await recordMidflightArrival(page, [ARRIVAL("arrival after native landing")], { mode: "after-end" });
    expect(proof.attachedBeforeEnd, `negative control attached before the end: ${proof.timeline}`).toBe(false);
    expect(proof.midflightAtAttachment, `negative control unexpectedly looked mid-flight: ${proof.timeline}`).toBe(false);
  });

  test("a second click asks for the end as it is then", async ({ page }) => {
    const { parkedAt } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    await callRiverTest(page, "appendMessage", ARRIVAL("arrival before the second click"));
    await expect(page.getByText("arrival before the second click")).toBeAttached({ timeout: 5_000 });
    await viewAtRest(page, "the first navigation should come to rest");
    expect(await distanceFromBottom(page), "premise: the arrival is below the first landing").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );

    await button(page).click();
    await expectSettledAtBottom(page, "the second click did not reach the end as it was then");
    await viewAtRest(page, "the second navigation should come to rest");
    expect(await scrollRequests(page), "one smooth request per click").toEqual({ smooth: 2, other: 0 });
  });

  test("a click replacing one still in flight wins, and nothing left from the first moves the view", async ({
    page,
  }) => {
    const { parkedAt } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    await callRiverTest(page, "appendMessage", ARRIVAL("arrival between the clicks"));
    await expect(page.getByText("arrival between the clicks")).toBeAttached({ timeout: 5_000 });
    await button(page).click();
    await expectSettledAtBottom(page, "the replacing click did not reach the end as it was then");
    await viewAtRest(page, "the view should come to rest");
    const landed = await newestVisibleRow(page);
    await page.waitForTimeout(1_500);
    await expectRowHolds(page, landed!, "something left over from the first click moved the view");
  });

  test("the reader's input stops it where it is, and that position is preserved", async ({ page }) => {
    const { parkedAt } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    await page.evaluate(() =>
      document
        .getElementById("chat-scroll-container")!
        .dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -1 })),
    );
    const rest = await viewAtRest(page, "the view should stop after the reader's input");
    expect(await distanceFromBottom(page), "the reader's input should have stopped it short").toBeGreaterThan(200);
    expect(rest).toBeGreaterThan(parkedAt);

    const stopped = await newestVisibleRow(page);
    expect(stopped, "premise: a message should be visible").not.toBeNull();
    await page.waitForTimeout(1_500);
    await deliver(page, ARRIVAL("arrival after the interruption"));
    await expectRowHolds(page, stopped!, "the interrupted position was not preserved");
  });

  test("deleting the reading anchor during the animation places at the latest message once", async ({ page }) => {
    const { parkedAt, destination } = await parkAndRecord(page);
    // At the navigation's first native `scroll`, delete the visible row and
    // every row after it but KEEP_LATEST, so the deferred deletion leaves the
    // view past the new end whenever it renders. An immediate arrival exposes
    // an animation that survives the landing. The fixture never writes
    // scrollTop or sends input, but a browser clamp or same-offset placement
    // may itself stop the animation; this test proves the landing holds, not
    // that the explicit stop caused it.
    //
    // Every listener is installed before the click. "During" is decided by
    // event order at the removal: native progress before it, no `scrollend`
    // before it, and the last `scroll` before it short of the destination
    // (the offset read after the removal is already clamped).
    const run = await page.evaluate(
      ({ keep, arrival }) =>
        new Promise<{
          ids: string[];
          unmatched: string[];
          requestedAt: number;
          progressBeforeRemoval: number;
          lastBeforeRemoval: number;
          endedBeforeRemoval: boolean;
          maxThen: number;
          landed: { id: string; gap: number } | null;
          landedAt: number;
          timeline: string;
        }>((resolve, reject) => {
          const c = document.getElementById("chat-scroll-container")!;
          const start = c.scrollTop;
          const t0 = performance.now();
          const timeline: string[] = [];
          const log = (kind: string, top = c.scrollTop) =>
            timeline.push(`${kind}@${top.toFixed(1)}+${(performance.now() - t0).toFixed(0)}ms`);
          let observer: MutationObserver | null = null;
          const cleanup = () => {
            clearTimeout(timer);
            observer?.disconnect();
            c.removeEventListener("scroll", onScroll);
            c.removeEventListener("scrollend", onEnd);
          };
          const timer = setTimeout(() => {
            cleanup();
            reject(new Error(`the deletion never landed at the end: ${timeline.join(" ")}`));
          }, 8_000);
          let removed: { maxThen: number; lastBeforeRemoval: number; progressBeforeRemoval: number } | null = null;
          let endedBeforeRemoval = false;
          let lastScroll = start;
          let ids: string[] = [];
          let requestedAt = Number.NaN;
          let unmatched: string[] | null = null;
          // Once the removal has rendered, wait for the view to reach the end
          // (the clamp and the app's placement), then deliver at once.
          const awaitLanding = () => {
            if (c.scrollHeight - c.scrollTop - c.clientHeight > 4) return void requestAnimationFrame(awaitLanding);
            const landed = window.__riverHistoryGeometry!.newestVisible(c);
            const landedAt = c.scrollTop;
            log("landed");
            window.__riverTest!.appendMessage(arrival);
            cleanup();
            resolve({
              ids,
              unmatched: unmatched!,
              requestedAt,
              ...removed!,
              endedBeforeRemoval,
              landed,
              landedAt,
              timeline: timeline.join(" "),
            });
          };
          const request = () => {
            const box = c.getBoundingClientRect();
            const rows = Array.from(c.querySelectorAll<HTMLElement>('[id^="msg-"]'));
            const first = rows.findIndex((r) => {
              const rect = r.getBoundingClientRect();
              return rect.bottom > box.top && rect.top < box.bottom;
            });
            ids = rows.slice(first, rows.length - keep).map((r) => r.id);
            requestedAt = c.scrollTop;
            log("request");
            observer = new MutationObserver(() => {
              if (removed || ids.some((id) => document.getElementById(id))) return;
              removed = {
                lastBeforeRemoval: lastScroll,
                progressBeforeRemoval: lastScroll - start,
                maxThen: c.scrollHeight - c.clientHeight,
              };
              log("removed");
              if (unmatched) requestAnimationFrame(awaitLanding);
            });
            observer.observe(document.getElementById("chat-content")!, { childList: true, subtree: true });
            window.__riverTest!.removeMessages(ids).then((u) => {
              unmatched = u;
              if (removed) requestAnimationFrame(awaitLanding);
            });
          };
          // After the app's own listeners: the app has had each event.
          const onScroll = () => {
            log("scroll");
            if (removed) return;
            lastScroll = c.scrollTop;
            if (Number.isNaN(requestedAt) && c.scrollTop > start + 1) request();
          };
          const onEnd = () => {
            log("end");
            if (!removed) endedBeforeRemoval = true;
          };
          c.addEventListener("scroll", onScroll);
          c.addEventListener("scrollend", onEnd);
          log("click");
          document.querySelector<HTMLElement>('[data-testid="scroll-to-bottom"]')!.click();
        }),
      { keep: KEEP_LATEST, arrival: ARRIVAL("arrival right after the landing") },
    );
    const what = `${JSON.stringify(run)}; parked at ${parkedAt}; destination ${destination}`;
    test.info().annotations.push({ type: "deletion order", description: what });
    expect(run.unmatched, `premise: every id named a message (${what})`).toEqual([]);
    expect(run.ids.length, `premise: the visible message and those after it were removed (${what})`).toBeGreaterThan(1);
    expect(
      run.progressBeforeRemoval,
      `premise: the animation had moved the view before the deletion rendered (${what})`,
    ).toBeGreaterThan(1);
    expect(run.endedBeforeRemoval, `premise: the animation had not ended before the deletion rendered (${what})`).toBe(
      false,
    );
    expect(
      destination - run.lastBeforeRemoval,
      `premise: the last scroll before the deletion rendered was short of the destination (${what})`,
    ).toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
    // The animation only moves down, so an end above where it was at the
    // request is one the view had passed: the browser clamped it there.
    expect(run.maxThen, `premise: the removal left the view past the new end, to be clamped (${what})`).toBeLessThan(
      run.requestedAt,
    );
    expect(run.landed, `premise: the landing has a visible message (${what})`).not.toBeNull();
    expect(run.ids, "the deleted anchor is not the landing").not.toContain(run.landed!.id);

    await expect(page.getByText("arrival right after the landing")).toBeAttached({ timeout: 5_000 });
    await viewAtRest(page, "the view should come to rest");
    await expectRowHolds(page, run.landed!, `the old animation carried the landing down to the arrival (${what})`);
    expect(await distanceFromBottom(page), "the arrival right after the landing was followed").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );
    expect(await scrollRequests(page), "the old request is not replaced by another smooth scroll").toEqual({
      smooth: 1,
      other: 0,
    });

    await page.waitForTimeout(1_500);
    await expectRowHolds(page, run.landed!, "a stale navigation end moved the landing");
    await deliver(page, ARRIVAL("arrival after the anchor was deleted mid-flight"));
    await expectRowHolds(page, run.landed!, "an arrival after the landing moved the view");
  });

  test("with reduced motion it goes straight to the end", async ({ page }) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await parkAndRecord(page);
    await button(page).click();
    expect(await distanceFromBottom(page), "reduced motion should jump, not animate").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    expect((await scrollRequests(page)).smooth, "no animation was requested").toBe(0);
  });
});

test.describe("A navigation's native end handles a reflow before it captures", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  /// Park, click, and arrange for `kind` at the native end. With `arrival`,
  /// the latest message moves below the click-time destination mid-flight.
  async function reflowAtNativeEnd(page: Page, kind: EndReflowKind, arrival = false) {
    const { parkedAt, destination } = await parkAndRecord(page);
    await orderRecorderStart(page);
    await endReflowAtNextScrollend(page, kind, GROW_PX);
    await button(page).click();
    if (arrival) await callRiverTest(page, "appendMessage", ARRIVAL("arrival mid-flight"));
    await animationUnderway(page, parkedAt);
    await expect
      .poll(() => endReflowResult(page), { timeout: 10_000, message: "premise: the navigation's end arrived" })
      .not.toBeNull();
    const run = (await endReflowResult(page)) as EndReflow;
    const log = await orderLog(page);
    const what = `${JSON.stringify(run)}; destination ${destination}; ${log}`;
    expect(log.slice(log.lastIndexOf(kind)), `premise: the end reached the app before the observer (${what})`).toMatch(
      new RegExp(`^${kind} end\\b.*\\bobserved\\b`),
    );
    expect(
      Math.abs(run.top - destination),
      `premise: the end came at the destination, so it can finish the navigation (${what})`,
    ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
    expect(run.anchor, `premise: a message was visible at the end (${what})`).not.toBeNull();
    return { run, what };
  }

  test("growth above the anchor at the end keeps the anchor's gap (controlled order: growth → end → observer)", async ({ page }) => {
    try {
      const { run, what } = await reflowAtNativeEnd(page, "grow");
      expect(
        Math.abs(run.shift - GROW_PX),
        `premise: the growth pushed the anchor down before the app saw it (${what})`,
      ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      await afterLayoutSettles(page);
      await expectVisibleRowHolds(page, run.anchor!, `the end captured the displaced view (${what})`);
      await deliver(page, ARRIVAL("arrival after the corrected end"));
      await expectVisibleRowHolds(page, run.anchor!, "an arrival after the corrected end moved the view");
      expect(await scrollRequests(page), "nothing re-issued the animation").toEqual({ smooth: 1, other: 0 });
    } finally {
      await orderRecorderStop(page);
    }
  });

  test("an anchor missing at the end, with rows still rendered, lands at the latest message", async ({ page }) => {
    try {
      const { run, what } = await reflowAtNativeEnd(page, "hide", true);
      expect(run.max - run.top, `premise: the arrival put the latest message below the end (${what})`).toBeGreaterThan(
        AT_BOTTOM_EPSILON_PX,
      );
      expect(
        await page.locator("#chat-content .anchor-row").count(),
        "premise: the rows are still rendered",
      ).toBeGreaterThan(0);
      await expectSettledAtBottom(page, `a missing anchor at the end should land at the latest message (${what})`);
      await viewAtRest(page, "the view should come to rest at the latest message");
      const landed = await newestVisibleRow(page);
      expect(landed, "premise: a message is visible at the landing").not.toBeNull();
      await endReflowUnhide(page);
      await deliver(page, ARRIVAL("arrival after the landing"));
      await expectVisibleRowHolds(page, landed!, "an arrival after the landing moved the view");
      expect(await distanceFromBottom(page), "the arrival was followed").toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
      expect(await scrollRequests(page), "the landing is not another animation").toEqual({ smooth: 1, other: 0 });
    } finally {
      await endReflowUnhide(page);
      await orderRecorderStop(page);
    }
  });
});

test.describe("A reflow correction that clamps still stops the animation", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a negative correction at scrollTop 0 cancels the navigation it interrupts", async ({ page, browserName }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    // A short history allows Firefox to start animating while its offset is still zero.
    for (let i = 0; i < 6; i++) await deliver(page, `clamp filler ${i}: ${"z ".repeat(300)}`);
    await readerScrollsWithoutGesture(page, 0);
    await afterLayoutSettles(page);
    await expect(button(page), "premise: the button is offered at the top").toBeVisible();
    await recordScrollRequests(page);

    // The reflow has to reach the app while the view is still at 0, so the
    // correction clamps to the offset it is already at. Chromium and WebKit
    // get it in the click's own task (frame 0): nothing has moved yet by
    // construction, and CI's Linux WebKit had moved by frame one on every try.
    // Firefox waits for frame three: its defect is that a write to the current
    // offset does not abort an ANIMATING smooth scroll, and it can still be at
    // zero then with the animation running. Measured with the explicit stop
    // removed (2026-10-03, 10 runs each): frame one caught it 0 times, frame
    // two 5, frame three 10. Chromium and WebKit abort on that write anyway, so
    // there the test checks the outcome, not the stop.
    //
    // Where the view has already moved by then, the reflow still runs, sized
    // to clamp: rows from the top of the view collapse until the correction
    // (where the view is plus the anchor's shift) is below 0. That try asserts
    // the view stops at the clamped correction and stays, then the setup runs
    // again for the same-offset case. Assertion failures are never retried.
    const frame = browserName === "firefox" ? 3 : 0;
    let movedTries = 0;
    for (let attempt = 1; attempt <= MAX_TRIES; attempt++) {
      if (attempt > 1) {
        await page.evaluate(() => {
          for (const row of document.querySelectorAll<HTMLElement>("#chat-content [data-anchor-row]")) {
            row.style.removeProperty("display");
          }
        });
        await afterLayoutSettles(page);
        await readerScrollsWithoutGesture(page, 0);
        await afterLayoutSettles(page);
      }
      const run = await page.evaluate(
        (frame) =>
          new Promise<Record<string, number>>((resolve) => {
            const c = document.getElementById("chat-scroll-container")!;
            const topAtClick = c.scrollTop;
            const before = window.__riverScrollRequests!.smooth;
            (document.querySelector('[data-testid="scroll-to-bottom"]') as HTMLElement).click();
            const smooth = window.__riverScrollRequests!.smooth - before;
            // Frames counted from the click; frame 0 is the click's own task.
            let frames = 0;
            const tick = () => {
              if (frames < frame && ++frames < frame) return void requestAnimationFrame(tick);
              const box = c.getBoundingClientRect();
              const visible = Array.from(c.querySelectorAll<HTMLElement>("#chat-content [data-anchor-row]")).filter(
                (r) => {
                  const rect = r.getBoundingClientRect();
                  return rect.bottom > box.top && rect.top < box.bottom;
                },
              );
              // Never the newest two visible rows: the app's anchor is the
              // newest visible row as of its last capture, which a view that
              // moved since may have scrolled past by one.
              const anchor = visible.at(-1)!;
              const collapsible = visible.slice(0, -2).filter((r) => r.id.startsWith("msg-"));
              const topBefore = c.scrollTop;
              const anchorBefore = anchor.getBoundingClientRect().top;
              let shift = 0;
              let collapsed = 0;
              for (const row of collapsible) {
                row.style.display = "none";
                collapsed++;
                shift = anchor.getBoundingClientRect().top - anchorBefore;
                if (topBefore + shift < 0) break;
              }
              c.dispatchEvent(new Event("scroll"));
              resolve({
                topAtClick,
                smooth,
                frames,
                topBefore,
                shift,
                collapsed,
                topAfterCorrection: c.scrollTop,
                max: c.scrollHeight - c.clientHeight,
              });
            };
            if (frame === 0) tick();
            else requestAnimationFrame(tick);
          }),
        frame,
      );
      const what = `attempt ${attempt}: ${JSON.stringify(run)}`;
      expect(run.topAtClick, `premise: the view starts at the top (${what})`).toBe(0);
      expect(run.smooth, `premise: the click started a smooth scroll (${what})`).toBe(1);
      expect(run.frames, `premise: the reflow came in the intended frame (${what})`).toBe(frame);
      expect(run.shift, `premise: the reflow moved the anchor up (${what})`).toBeLessThan(-IN_PLACE_TOLERANCE_PX);
      expect(
        run.topBefore + run.shift,
        `premise: the correction is below 0, so the browser clamps it to 0 (${what})`,
      ).toBeLessThan(0);
      expect(run.topAfterCorrection, `premise: the correction clamped to 0 (${what})`).toBe(0);
      expect(run.max, `premise: there is far to go (${what})`).toBeGreaterThan(1_000);
      if (frame === 0) {
        expect(run.topBefore, `premise: nothing moves in the click's own task (${what})`).toBe(0);
      }

      await afterLayoutSettles(page);
      const rest = await viewAtRest(page, `the view should be stopped (${what})`);
      expect(rest, `the animation kept going after the reflow cancelled it (${what})`).toBeLessThanOrEqual(
        AT_BOTTOM_EPSILON_PX,
      );
      await page.waitForTimeout(500);
      expect(await scrollTop(page), `the animation resumed later (${what})`).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
      expect(await scrollRequests(page), "one smooth request per click").toEqual({ smooth: attempt, other: 0 });
      if (run.topBefore === 0) {
        test.info().annotations.push({
          type: "clamp cancel",
          description: `same offset on ${what}; moved tries ${movedTries}`,
        });
        return;
      }
      // A real move to 0 aborts in every engine; it is checked, but it is not
      // the same-offset case this test exists for.
      movedTries++;
    }
    throw new Error(
      `premise: the view had already moved in frame ${frame} on all ${MAX_TRIES} tries (each was checked at its clamp)`,
    );
  });
});

test.describe("Hiding the chat cancels the animation", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  for (const hide of [
    { by: "the rooms button", opener: "hamburger-rooms-button", back: "rooms-back-button" },
    { by: "the members button", opener: "header-members-button", back: "members-back-button" },
  ] as const) {
    test(`hidden mid-flight by ${hide.by}, with arrivals while hidden, it does not resume on reveal`, async ({
      page,
    }) => {
      const { parkedAt } = await parkAndRecord(page);
      await button(page).click();
      await animationUnderway(page, parkedAt);
      await hideChat(page, hide.opener);
      for (let i = 0; i < 3; i++) {
        const text = ARRIVAL(`hidden arrival ${i}`);
        await callRiverTest(page, "appendMessage", text);
        await expect(page.getByText(text)).toBeAttached({ timeout: 5_000 });
      }
      // Longer than any quiet interval the navigation could have left behind.
      await page.waitForTimeout(1_500);

      await revealChat(page, hide.back);
      const revealed = await newestVisibleRow(page);
      expect(revealed, "premise: a message should be visible").not.toBeNull();
      expect(await distanceFromBottom(page), "the reveal should not have finished the animation").toBeGreaterThan(
        200,
      );
      await expectRowHolds(page, revealed!, "the animation resumed, or something moved the view, after the reveal");
      expect((await scrollRequests(page)).smooth, "nothing re-issued the animation").toBe(1);
    });
  }
});
