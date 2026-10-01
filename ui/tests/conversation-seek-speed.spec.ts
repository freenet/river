import { test, expect, Page } from "@playwright/test";
import {
  SEEK_CLOCK_FRAME_MS,
  SeekClockGuard,
  SeekClockRecord,
  seekClockInstall,
  seekClockLog,
  seekClockNativeRun,
  seekClockPause,
  seekClockRun,
} from "./history-scroll-fixture";
import {
  AT_BOTTOM_EPSILON_PX,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  endMinus,
  expectSettledAtBottom,
  openRoomAtBottom,
  readerScrollsWithoutGesture,
  scrollTop,
} from "./history-scroll-helpers";

// The scroll-to-latest button's animation must keep its speed when a message
// lands mid-flight (moved here from conversation-autoscroll.spec.ts).
//
// Chromium restarted the native smooth scroll's ease-in every time it was
// re-aimed, so a message landing mid-flight dropped it from ~140px to ~5px a
// frame (evidence/seek-stall-probe in the plans). The app now animates with its
// own frame loop, and this spec is what holds it to that.
//
// The speed regression runs on Playwright's clock, one 16ms frame at a time
// (history-scroll-fixture.ts, "seek clock"), so how many frames it gets to
// compare no longer depends on how busy the machine is: sampled at the host's
// cadence, Linux CI WebKit had 1/1/0 usable frames before the end, and mobile
// Safari's long frames used most of the trip before the arrival's guard ran.
// The native-clock smoke test keeps the browser's own scheduling covered: a
// real arrival mid-flight is delivered and the animation still ends at the
// new end. It makes no speed claim.
//
// Assumes the example-data build (`window.__riverTest`). Arrivals are INBOUND:
// sending through the composer forces a snap and would prove nothing.

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
const BOTTOM_THRESHOLD_PX = 100;

/// Tall fillers, and how far above the end the reader parks before the press.
const SEEK_SPEED_FILLERS = 30;
const SEEK_SPEED_PARK_PX = 7_000;
/// The arrival: requested once the animation has moved for three frames (three
/// speeds to compare against) and travelled this far, and only if the view is
/// still more than SEEK_SPEED_ARRIVE_ABOVE_PX above the end.
const SEEK_SPEED_ARRIVE_AFTER_PX = 1_500;
const SEEK_SPEED_ARRIVE_ABOVE_PX = 1_500;
/// Where the speed check stops: an ease-out is slow at the very end on purpose.
const SEEK_SPEED_NEAR_END_PX = 300;
/// A frame slower than a quarter of the median of the three before it is a stall.
const SEEK_SPEED_MIN_RATIO = 1 / 4;
/// How long the whole trip may take, on the controlled clock.
const SEEK_SPEED_BUDGET_MS = 1_200;

const ARRIVAL = (what: string) => `arrival during the ${what} animation: ${"v".repeat(200)}`;

/// Fill Team Chat Room with tall rows and park the reader SEEK_SPEED_PARK_PX
/// above the end, clear of the backfill strip at the top.
async function parkFarAboveTheEnd(page: Page) {
  await openRoomAtBottom(page, "Team Chat Room");
  await page.evaluate(async (count) => {
    for (let i = 0; i < count; i++) {
      (window as any).__riverTest.appendMessage(`speed filler ${i}: ${"w ".repeat(450)}`);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    }
  }, SEEK_SPEED_FILLERS);
  await expect(page.getByText(`speed filler ${SEEK_SPEED_FILLERS - 1}:`)).toBeAttached({ timeout: 5_000 });
  await expectSettledAtBottom(page, "premise: the fillers should have been followed");
  await readerScrollsWithoutGesture(page, await endMinus(page, SEEK_SPEED_PARK_PX));
  await afterLayoutSettles(page);
  expect(await distanceFromBottom(page), "premise: the reader should be parked far up").toBeGreaterThan(
    SEEK_SPEED_PARK_PX - BOTTOM_THRESHOLD_PX,
  );
  expect(await scrollTop(page), "premise: parked clear of the backfill strip").toBeGreaterThan(800);
  await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({ timeout: 5_000 });
}

/// The arrival was really requested, and its row really observed after that.
/// An absent delivery fails here rather than being indexed as an arrival.
function expectDelivered(record: SeekClockRecord, log: string) {
  expect(record.requestedAt, `premise: the arrival should have been requested mid-flight (${log})`).toBeGreaterThan(0);
  expect(record.patchAt, `premise: the arrival's row should have been observed in the history (${log})`).toBeGreaterThan(
    record.requestedAt,
  );
  const at = record.frames[record.requestedAt];
  expect(
    at.max - at.top,
    `premise: the arrival should be requested more than ${SEEK_SPEED_ARRIVE_ABOVE_PX}px from the end (${log})`,
  ).toBeGreaterThan(SEEK_SPEED_ARRIVE_ABOVE_PX);
  expect(record.patchMax, `premise: the arrival should have grown the history (${log})`).toBeGreaterThan(at.max);
}

/// The first frame at the end that the arrival's patch had already moved.
function reachedTheNewEnd(record: SeekClockRecord) {
  return record.frames.find(
    (f, k) => k >= record.patchAt && f.max >= record.patchMax && f.max - f.top <= AT_BOTTOM_EPSILON_PX,
  );
}

test.describe("The scroll-to-latest animation keeps its speed", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // Speeds are px/ms on the clock that also drives the app's frames. Each frame
  // after the patch is compared with the three before IT, not with a fixed speed
  // from before the arrival: an ease-out slows down as it gets close by design,
  // and only a sudden drop is a stall.
  test("the scroll-to-latest animation keeps its speed when a message lands mid-flight", async ({ page }) => {
    await seekClockInstall(page);
    await parkFarAboveTheEnd(page);
    await seekClockPause(page);

    const guard: SeekClockGuard = {
      text: ARRIVAL("fast"),
      minMovedFrames: 3,
      minTravelPx: SEEK_SPEED_ARRIVE_AFTER_PX,
      minDistancePx: SEEK_SPEED_ARRIVE_ABOVE_PX,
    };
    let record: SeekClockRecord;
    try {
      record = await seekClockRun(page, guard);
    } finally {
      await page.clock.resume();
    }
    const log = seekClockLog(record);
    test.info().annotations.push({ type: "seek frames", description: log });
    const { frames } = record;
    const pressedAt = frames[0].t;

    for (let k = 1; k < frames.length; k++) {
      expect(frames[k].t - frames[k - 1].t, `premise: the clock should step one frame at a time (${log})`).toBe(
        SEEK_CLOCK_FRAME_MS,
      );
    }
    expectDelivered(record, log);
    expect(record.requestedAt, `premise: the animation should have moved for three frames (${log})`).toBeGreaterThanOrEqual(3);

    const speed = (k: number) => (frames[k].top - frames[k - 1].top) / (frames[k].t - frames[k - 1].t);
    const median3 = (xs: number[]) => [...xs].sort((a, b) => a - b)[1];
    let checked = 0;
    for (let k = record.patchAt; k < frames.length; k++) {
      if (frames[k].max - frames[k].top <= SEEK_SPEED_NEAR_END_PX) break;
      const before = median3([speed(k - 3), speed(k - 2), speed(k - 1)]);
      expect(
        speed(k),
        `the animation stalled ${Math.round(frames[k].t - pressedAt)}ms after the press: ` +
          `${speed(k).toFixed(2)}px/ms against ${before.toFixed(2)}px/ms just before (${log})`,
      ).toBeGreaterThanOrEqual(before * SEEK_SPEED_MIN_RATIO);
      checked++;
    }
    expect(checked, `premise: frames between the patch and the end should be checked (${log})`).toBeGreaterThanOrEqual(3);

    const reached = reachedTheNewEnd(record);
    expect(reached, `the animation never reached the end (${log})`).toBeDefined();
    expect(reached!.t - pressedAt, `the animation took too long to reach the end (${log})`).toBeLessThanOrEqual(
      SEEK_SPEED_BUDGET_MS,
    );

    await expectSettledAtBottom(page, "the view left the newest message after the animation ended");
    await deliver(page, "arrival after the fast animation");
    await expectSettledAtBottom(page, "the follow did not survive the animation");
  });

  // The browser's own clock and frame scheduling, which the controlled run above
  // replaces. Delivered mid-flight on the first frame that has moved, and the
  // animation must still finish at the end the patch moved.
  test("on the browser's own clock, a message landing mid-flight is delivered and the animation reaches the new end", async ({
    page,
  }) => {
    await parkFarAboveTheEnd(page);
    const record = await seekClockNativeRun(page, {
      text: ARRIVAL("native"),
      minMovedFrames: 1,
      minTravelPx: 1,
      minDistancePx: SEEK_SPEED_ARRIVE_ABOVE_PX,
    });
    const log = seekClockLog(record);
    test.info().annotations.push({ type: "seek frames", description: log });
    expectDelivered(record, log);
    expect(reachedTheNewEnd(record), `the animation never reached the new end (${log})`).toBeDefined();

    await expectSettledAtBottom(page, "the view left the newest message after the animation ended");
    await deliver(page, "arrival after the native animation");
    await expectSettledAtBottom(page, "the follow did not survive the animation");
  });
});
