import { test, expect, Page, Route } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { clampCleanup, clampOverhang, clampRowDrift, expectFinalEndClamp } from "./history-clamp-fixture";
import { seekClockInstall, seekClockPause } from "./history-scroll-fixture";
import {
  FollowEntry,
  RowPosition,
  followAwaitAfter,
  followBeforeSettleFire,
  followDeliver,
  followFrame,
  followGate,
  followGrowAbove,
  followGrowAboveThenEnd,
  followGrowContainerThenEnd,
  followLog,
  followMark,
  followRealFrames,
  followReaderMove,
  followRecorderStart,
  followRecorderStop,
  followResizeBeforeEnd,
  followResizeRecord,
  followSettleRecord,
  followTimeline,
  followUngate,
  newestVisibleRow,
  rowDrift,
} from "./history-follow-fixture";
import {
  missingAnchorDrift,
  missingAnchorHideScrollbar,
  missingAnchorRemove,
  missingAnchorSelect,
} from "./history-missing-anchor-fixture";
import {
  AT_BOTTOM_EPSILON_PX,
  BOTTOM_THRESHOLD_PX,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  endMinus,
  expectSettledAtBottom,
  openRoomAtBottom,
  parkAboveTheEnd,
  readerScrollsWithoutGesture,
  scrollTop,
  viewportHeight,
} from "./history-scroll-helpers";

// How the history's follow state moves between Free, Gesture and Seeking when
// the reader and our own work interleave (history_scroll.rs, "Follow states").
//
// Assumes the example-data build (`window.__riverTest`). Arrivals are INBOUND:
// sending through the composer forces a snap and would prove nothing.

/// Matches SCROLL_TOP_SLACK_PX in ui/src/components/conversation.rs: the most one move may be and still be
/// rounding. Only a premise here, so a policy change fails at setup.
const SCROLL_TOP_SLACK_PX = 2;
/// The geometry budget for "the reader's message did not move" (as in
/// conversation-autoscroll.spec.ts), not derived from the slack above.
const IN_PLACE_TOLERANCE_PX = 4;

test.describe("A pending layout clamp at settlement", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  for (const deliveredFirst of [false, true]) {
    test(`a signature-unchanged clamp before scrollend preserves the parked reader (controlled order: ${deliveredFirst ? "clamp → scroll → end" : "clamp → end → scroll"})`, async ({ page }) => {
      await seekClockInstall(page);
      await openRoomAtBottom(page, "Team Chat Room");
      for (let i = 0; i < 8; i++) await deliver(page, `clamp filler ${i}: ${"y".repeat(200)}`);
      await afterLayoutSettles(page);
      const overhang = await clampOverhang(page);
      try {
        await readerScrollsWithoutGesture(page, overhang.after.max);
        await afterLayoutSettles(page);
        await seekClockPause(page);
        await followRecorderStart(page, { gateEnds: true });
        await followMark(page, "park");
        const move = await followReaderMove(page, -400);
        expect(move.delivered, "premise: the reader's move reached the app").toBe(true);
        expect(move.after - move.before, "premise: the upward move starts a held gesture").toBeCloseTo(-400, 0);
        expect(await followAwaitAfter(page, "park", "held"), "premise: the move's end was gated").toBe(true);
        expect(await distanceFromBottom(page), "premise: the reader is outside the follow band").toBeGreaterThan(BOTTOM_THRESHOLD_PX);
        const at = await newestVisibleRow(page);
        expect(at, "premise: the original anchor row is visible").not.toBeNull();

        const settled = await page.evaluate(async (deliveredFirst) => {
          const rec = window.__followRecorder!;
          if (!deliveredFirst) rec.setGate(false);
          rec.log.push("clamp");
          const removed = window.__historyClamp.remove();
          if (deliveredFirst) {
            // Let the real scroll reach the app while its automatic end stays gated.
            for (let i = 0; i < 3; i++) await new Promise<void>((r) => rec.afterRealFrame(r));
            rec.setGate(false);
          }
          const scrollsBeforeEnd = window.__historyClamp.scrolls;
          document.getElementById("chat-scroll-container")!.dispatchEvent(new Event("scrollend"));
          return { removed, scrollsBeforeEnd, scrollsAfterEnd: window.__historyClamp.scrolls, log: rec.log.slice() };
        }, deliveredFirst);
        expectFinalEndClamp(settled.removed);
        if (deliveredFirst) {
          expect(settled.scrollsBeforeEnd, "premise: the clamp scroll arrived before the end").toBeGreaterThan(settled.removed.scrolls);
        } else {
          expect(settled.scrollsBeforeEnd, "premise: the end precedes the clamp scroll").toBe(settled.removed.scrolls);
        }
        expect(settled.scrollsAfterEnd, "premise: dispatching the end did not deliver another scroll").toBe(settled.scrollsBeforeEnd);
        if (!deliveredFirst) {
          expect(settled.log.slice(settled.log.lastIndexOf("clamp")), "premise: the end reached the app immediately after the clamp").toEqual(["clamp", "end"]);
        } else {
          expect(settled.log.slice(settled.log.lastIndexOf("clamp")).join(" "), "premise: the clamp scroll reached the app before settlement").toMatch(/^clamp scroll (held )*end$/);
        }
        await followRealFrames(page, 3);
        if (!deliveredFirst) {
          expect(await page.evaluate(() => window.__historyClamp.scrolls), "premise: the later scroll was delivered").toBeGreaterThan(settled.scrollsAfterEnd);
        }
        expect(await scrollTop(page), "the unreachable anchor leaves the view at the clamped end").toBeCloseTo(settled.removed.after.top, 0);

        const marker = "arrival after native clamp settle";
        await followDeliver(page, `${marker}\n${Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n")}`);
        const height = await page.getByText(marker, { exact: false }).last().evaluate((el) => el.getBoundingClientRect().height);
        expect(height, "premise: the arrival restores the lost range plus the follow band").toBeGreaterThan(500);
        await expect.poll(() => clampRowDrift(page, at!), {
          message: "settlement captured the clamp and followed the tall arrival",
        }).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
        await followRealFrames(page, 3);
        expect(await clampRowDrift(page, at!), "the saved row stays at its gap after later events").toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
        expect(await distanceFromBottom(page), "the arrival must leave the reader parked").toBeGreaterThan(BOTTOM_THRESHOLD_PX);
      } finally {
        await followRecorderStop(page);
        await clampCleanup(page);
        await page.clock.resume();
      }
    });
  }
});

test.describe("An unreachable anchor on mobile reveal", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  for (const deadlinePassed of [false, true]) {
    test(`a hidden clamp preserves the parked reader on reveal ${deadlinePassed ? "after" : "before"} the quiet deadline`, async ({ page }) => {
      await seekClockInstall(page);
      await openRoomAtBottom(page, "Team Chat Room");
      for (let i = 0; i < 8; i++) await deliver(page, `hidden clamp filler ${i}: ${"y".repeat(200)}`);
      await afterLayoutSettles(page);
      const overhang = await clampOverhang(page);
      try {
        await readerScrollsWithoutGesture(page, overhang.after.max);
        await afterLayoutSettles(page);
        await seekClockPause(page);
        await followRecorderStart(page, { gateEnds: true, observeSettle: true });
        const move = await followReaderMove(page, -400);
        expect(move.delivered, "premise: the held reader scroll reached the app").toBe(true);
        expect(await distanceFromBottom(page)).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
        // Produce a real anchor correction, then refuse its end to arm the deadline.
        await followGrowAbove(page, 300);
        await followRealFrames(page, 3);
        const at = await newestVisibleRow(page);
        expect(at, "premise: the saved row is visible before hiding").not.toBeNull();
        await page.evaluate(() => {
          window.__followRecorder!.setGate(false);
          document.getElementById("chat-scroll-container")!.dispatchEvent(new Event("scrollend"));
          window.__followRecorder!.setGate(true);
        });
        const armed = await followSettleRecord(page);
        expect(armed.settle.filter((t) => t.cleared === null && t.fired === null), "premise: refusing the correction end armed the reader's deadline").toHaveLength(1);
        const before = await page.evaluate(() => window.__historyClamp.snapshot());
        await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
        await page.clock.runFor(0);
        await expect(page.locator("#chat-scroll-container")).toBeHidden();
        expect(await viewportHeight(page), "premise: hidden geometry is not measured").toBe(0);
        await followRealFrames(page, 2);
        await page.evaluate(() => window.__historyClamp.remove());
        if (deadlinePassed) await page.clock.runFor(120);
        await page.getByTestId("rooms-back-button").click();
        // Nested zero-delay work can be scheduled one clock millisecond later.
        // Let the reveal render, then prove it still preceded the quiet deadline.
        await expect.poll(async () => {
          await page.clock.runFor(1);
          return viewportHeight(page);
        }, { message: "premise: the back button reveals the history" }).toBeGreaterThan(0);
        await expect(page.locator("#chat-scroll-container")).toBeVisible();
        await followRealFrames(page, 3);
        const after = await page.evaluate(() => window.__historyClamp.snapshot());
        test.info().annotations.push({
          type: "hidden clamp geometry",
          description: JSON.stringify({ before, after, log: await followLog(page), timers: await followSettleRecord(page) }),
        });
        expectFinalEndClamp({ before, after, scrolls: 0 });
        const settled = await followSettleRecord(page);
        if (!deadlinePassed) {
          expect(settled.now, "premise: reveal finished before the old deadline").toBeLessThan(armed.settle[0].at + armed.settle[0].delay);
        } else {
          expect(settled.settle[0].fired, "premise: the old deadline fired while hidden").not.toBeNull();
        }
        expect(settled.settle.filter((t) => t.cleared === null && t.fired === null), "reveal completes the old gesture and cancels its deadline").toEqual([]);
        const marker = "tall arrival after hidden clamp";
        await followDeliver(page, `${marker}\n${Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n")}`);
        expect(await page.getByText(marker, { exact: false }).last().evaluate((el) => el.getBoundingClientRect().height)).toBeGreaterThan(500);
        expect(await clampRowDrift(page, at!), "reveal captured the clamp and followed the tall arrival").toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
        expect(await distanceFromBottom(page)).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
      } finally {
        await followRecorderStop(page);
        await clampCleanup(page);
        await page.clock.resume();
      }
    });
  }
});

/// The row is back at its gap, and stays there for five samples over 500ms.
async function expectRowHeld(page: Page, before: { id: string; gap: number }, why: string) {
  await expect.poll(() => rowDrift(page, before), { timeout: 5_000, message: why }).toBeLessThanOrEqual(
    IN_PLACE_TOLERANCE_PX,
  );
  const drifts: number[] = [];
  for (let i = 0; i < 5; i++) {
    await page.waitForTimeout(100);
    drifts.push(await rowDrift(page, before));
  }
  expect(Math.max(...drifts), `${why} (then drifted: ${drifts.join(", ")})`).toBeLessThanOrEqual(
    IN_PLACE_TOLERANCE_PX,
  );
}

// Direction during a seek is measured from one origin that our own frames
// shift, so the reader's slow 1px moves add up as they do in a gesture. Before,
// each move was compared with the frame just recorded and never got past the
// slack, and the seek dragged the reader to the end.
test.describe("A reader taking over the scroll-to-latest animation", () => {
  test.use({ viewport: { width: 1280, height: 900 } });
  const PARK_PX = 7_000;
  /// Frames before the first move, so the animation is visibly under way.
  const LEAD_FRAMES = 2;
  const MOVES = 4;
  /// Frames after the moves: far more than a 7,000px trip needs.
  const TAIL_FRAMES = 45;

  test("1px upward moves between the animation's frames add up and take over (controlled order: frame → move → frame)", async ({
    page,
  }) => {
    await seekClockInstall(page);
    await parkAboveTheEnd(page, PARK_PX);
    await seekClockPause(page);

    const pressedAt = await scrollTop(page);
    let entries: FollowEntry[] = [];
    try {
      await followRecorderStart(page);
      await page.getByTestId("scroll-to-bottom").click();
      for (let i = 0; i < LEAD_FRAMES; i++) await followFrame(page);
      for (let i = 0; i < MOVES; i++) {
        await followReaderMove(page, -1);
        await followFrame(page);
      }
      for (let i = 0; i < TAIL_FRAMES; i++) await followFrame(page);
    } finally {
      entries = await followRecorderStop(page);
      await page.clock.resume();
    }
    const timeline = followTimeline(entries);
    test.info().annotations.push({ type: "frames and moves", description: timeline });

    const inputs = entries.flatMap((e) => (e.kind === "input" ? [e] : []));
    const lead = entries[LEAD_FRAMES - 1];
    expect(
      lead.kind === "frame" ? lead.top - pressedAt : NaN,
      `premise: the animation should be under way before the first move (${timeline})`,
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    expect(inputs.length, `premise: every move should be made (${timeline})`).toBe(MOVES);
    for (const input of inputs) {
      const moved = input.after - input.before;
      expect(input.delivered, `premise: each move's scroll event should reach the app (${timeline})`).toBe(true);
      expect(moved, `premise: each move should go up (${timeline})`).toBeLessThan(0);
      expect(
        -moved,
        `premise: no single move should be past rounding on its own (${timeline})`,
      ).toBeLessThanOrEqual(SCROLL_TOP_SLACK_PX);
      expect(
        input.max - input.before,
        `premise: each move should land while the animation is mid-flight (${timeline})`,
      ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    }
    const total = inputs.reduce((sum, i) => sum + (i.after - i.before), 0);
    expect(-total, `premise: together the moves should be past rounding (${timeline})`).toBeGreaterThan(
      SCROLL_TOP_SLACK_PX,
    );

    const last = inputs[inputs.length - 1];
    expect(
      await distanceFromBottom(page),
      `the animation carried the reader to the end through their upward moves (${timeline})`,
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    expect(
      Math.abs((await scrollTop(page)) - last.after),
      `the view moved after the reader took over (${timeline})`,
    ).toBeLessThanOrEqual(1);

    await afterLayoutSettles(page);
    const parked = await newestVisibleRow(page);
    expect(parked, "premise: a message should be visible").not.toBeNull();
    await deliver(page, `arrival after a slow takeover: ${"u".repeat(200)}`);
    await expectRowHeld(page, parked!, `an arrival after the takeover moved the reader (${timeline})`);
  });

  // Real wheel ticks on the browser's own clock, against whatever order the
  // engine chooses. The recorder samples the view in an animation frame queued
  // after the app's, so a `scroll` event's distance from that sample is the
  // reader's part of it, as the app sees it. Whether the ticks add up past
  // rounding before the end is the engine's business; the outcome has to match
  // what they did.
  test("native input smoke: small wheel ticks during the animation", async ({ page, isMobile }) => {
    test.skip(
      isMobile,
      "no small-wheel input on the mobile projects: page.mouse.wheel is unsupported on mobile WebKit, and mobile Chromium is a touch device",
    );
    await parkAboveTheEnd(page, PARK_PX);
    const box = (await page.locator("#chat-scroll-container").boundingBox())!;
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      const log: string[] = [];
      const rec = { log, readerUp: 0, upMidFlight: 0, wheelsMidFlight: 0, live: true };
      let post = c.scrollTop;
      let sampling = false;
      const left = () => c.scrollHeight - c.clientHeight - c.scrollTop;
      const sample = () => {
        post = c.scrollTop;
        if (rec.live) requestAnimationFrame(sample);
      };
      c.addEventListener("wheel", (e) => {
        log.push(`w${e.deltaY}(${Math.round(left())})`);
        if (left() > 100) rec.wheelsMidFlight++;
      });
      c.addEventListener("scroll", () => {
        if (!sampling) {
          // The first frame's event. The app asked for its next frame in that
          // frame, so this request, made after it, runs after the app's.
          sampling = true;
          post = c.scrollTop;
          requestAnimationFrame(sample);
        }
        const reader = c.scrollTop - post;
        post = c.scrollTop;
        log.push(`s${reader >= 0 ? "+" : ""}${+reader.toFixed(2)}(${Math.round(left())})`);
        if (reader < 0) {
          rec.readerUp -= reader;
          if (left() > 100) rec.upMidFlight -= reader;
        }
      });
      (window as unknown as { __followWheel: typeof rec }).__followWheel = rec;
      (document.querySelector('[data-testid="scroll-to-bottom"]') as HTMLElement).click();
    });
    for (let i = 0; i < 12 && (await distanceFromBottom(page)) > BOTTOM_THRESHOLD_PX; i++) {
      await page.mouse.wheel(0, -1);
    }
    await afterLayoutSettles(page);
    const rec = await page.evaluate(() => {
      const r = (window as unknown as { __followWheel: { log: string[]; readerUp: number; upMidFlight: number; wheelsMidFlight: number; live: boolean } }).__followWheel;
      r.live = false;
      return r;
    });
    const timeline = `${rec.log.join(" ")}; reader up ${rec.readerUp}px, ${rec.upMidFlight}px of it mid-flight`;
    test.info().annotations.push({ type: "wheel ticks and scroll events", description: timeline });
    expect(rec.wheelsMidFlight, `premise: a wheel tick should land mid-flight (${timeline})`).toBeGreaterThan(0);

    const parked = (await distanceFromBottom(page)) > BOTTOM_THRESHOLD_PX;
    expect(
      parked,
      `the outcome does not match the reader's movement mid-flight (${timeline})`,
    ).toBe(rec.upMidFlight > SCROLL_TOP_SLACK_PX);
    if (parked) {
      const row = await newestVisibleRow(page);
      expect(row, "premise: a message should be visible").not.toBeNull();
      await deliver(page, `arrival after a wheel takeover: ${"u".repeat(200)}`);
      await expectRowHeld(page, row!, `an arrival after the wheel takeover moved the reader (${timeline})`);
    } else {
      await expectSettledAtBottom(page, `the animation did not end at the newest message (${timeline})`);
      await deliver(page, "arrival after the wheel ticks");
      await expectSettledAtBottom(page, `the follow did not survive the wheel ticks (${timeline})`);
    }
  });
});

/// A tall inbound message: more than the follow band on its own.
const TALL = (marker: string) => `${marker}\n${Array.from({ length: 12 }, (_, i) => `line ${i}`).join("\n")}`;

/// The fixture variant with rooms deeper than the render window.
const DEEP_ROOM_PATH = "/?deep-history-room=1";

/// The quiet interval a gesture settles after when no native end does
/// (SCROLL_SETTLE_DEBOUNCE_MS in history_scroll.rs).
const QUIET_MS = 120;
/// Up past rounding, and small enough that a short arrival keeps the reader
/// inside the band.
const CORRECTED_UP_PX = 20;
/// Growth above the reader's row: the correction it needs.
const GROW_PX = 300;
/// Clock time between the reader's move and the growth.
const READER_QUIET_MS = 60;

// A layout change above a held reader is corrected by writing `scrollTop`, and
// the browser answers that write with its own `scrollend`. Native events carry
// no write token, so the app cannot prove whose end it is; before, it took it
// for the reader's, settled the held gesture where it was, and the next short
// arrival snapped a reader who was still scrolling up inside the band. A
// matching end now leaves the gesture held, and the gesture settles at the
// reader's own quiet deadline instead (QUIET_MS after their last move, however
// much of our own work came after it).
//
// The clock is paused once the room is set up, so no app timer runs unless the
// test advances it; native rendering, `scroll` and `scrollend` still come from
// real frames. The reader's own move is a programmatic one, which every engine
// ends at once: a gate consumes that end, so the gesture stays unsettled as a
// held finger would, and is removed before the correction, whose end must reach
// the app.

/// Open a filled room and move up `upPx` with the move's native end gated away,
/// so the gesture is held and unsettled, as under a finger. Leaves the clock
/// paused and the recorder running, gated: the caller stops both in a
/// `finally`. The reader's newest visible message.
async function heldGesture(
  page: Page,
  { path = "/", upPx = CORRECTED_UP_PX, fillers = 8, observeSettle = false, initialShrinkPx = 0 } = {},
) {
  await seekClockInstall(page);
  await openRoomAtBottom(page, "Team Chat Room", path);
  for (let i = 0; i < fillers; i++) await deliver(page, `filler ${i}: ${"y".repeat(200)}`);
  await expectSettledAtBottom(page, "premise: the fillers should have been followed");
  if (initialShrinkPx) {
    await page.evaluate((px) => {
      const c = document.getElementById("chat-scroll-container")!;
      c.style.maxHeight = `${c.clientHeight - px}px`;
    }, initialShrinkPx);
    await afterLayoutSettles(page);
    await expectSettledAtBottom(page, "premise: the initial height constraint was followed");
  }
  await afterLayoutSettles(page);
  await seekClockPause(page);
  await followRecorderStart(page, { gateEnds: true, observeSettle });

  await followMark(page, "move");
  const move = await followReaderMove(page, -upPx);
  expect(move.delivered, "premise: the reader's scroll event reached the app").toBe(true);
  expect(move.after - move.before, "premise: the reader moved up").toBeCloseTo(-upPx, 0);
  expect(await followAwaitAfter(page, "move", "held"), "premise: the gate held the move's own end").toBe(true);
  await followRealFrames(page, 3);
  const at = await newestVisibleRow(page);
  expect(at, "premise: a message should be visible").not.toBeNull();
  return at!;
}

/// A `heldGesture`, then, `READER_QUIET_MS` later and with the gate removed,
/// growth above the reader, and wait for the correction's own end to reach the
/// app. Leaves the clock paused and the recorder running: the caller stops both
/// in a `finally`.
async function correctedHeldGesture(
  page: Page,
  { path = "/", upPx = CORRECTED_UP_PX, fillers = 8, observeSettle = false } = {},
) {
  const at = await heldGesture(page, { path, upPx, fillers, observeSettle });

  await page.clock.runFor(READER_QUIET_MS);
  expect(await followUngate(page), "premise: the gate was still in place").toBe(true);
  await followRealFrames(page, 3);
  const grow = await followGrowAbove(page, GROW_PX);
  expect(Math.abs(grow.after - grow.before), "premise: the growth did not clamp the view").toBeLessThanOrEqual(1);
  const ended = await followAwaitAfter(page, "grow", "end", 20);
  const log = await followLog(page);
  const correctedTop = await scrollTop(page);
  const correction = correctedTop - grow.before;
  const fromUngate = log.slice(log.lastIndexOf("ungated"));
  expect(fromUngate, `premise: nothing gated or ended between the ungate and the growth (${log})`).toMatch(
    /^ungated (scroll )*grow\b/,
  );
  expect(ended, `premise: the correction's own scrollend reached the app (${log})`).toBe(true);
  expect(fromUngate, `premise: the correction scrolled before its end (${log})`).toMatch(/grow .*scroll.* end/);
  expect(
    Math.abs(correction - GROW_PX),
    `premise: the restore corrected the view by the growth (${correction}px; ${log})`,
  ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
  expect(await rowDrift(page, at), `premise: the correction put the reader's message back (${log})`).toBeLessThanOrEqual(
    IN_PLACE_TOLERANCE_PX,
  );
  test.info().annotations.push({
    type: "correction",
    description: `${correction}px; ${at.id} at ${at.gap.toFixed(1)}px; ${log}`,
  });
  return { at, correctedTop };
}

/// Advance the clock to the reader's quiet deadline, QUIET_MS after their move.
const toQuietDeadline = (page: Page) => page.clock.runFor(QUIET_MS - READER_QUIET_MS);

async function expectInBand(page: Page, why: string) {
  expect(await distanceFromBottom(page), why).toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX - IN_PLACE_TOLERANCE_PX);
}

/// A short arrival after the correction's end is held, inside the band.
async function expectShortArrivalHeld(page: Page, at: RowPosition) {
  await followDeliver(page, "join");
  const log = await followLog(page);
  test.info().annotations.push({ type: "arrival", description: `${await distanceFromBottom(page)}px above the end; ${log}` });
  await expectRowHeld(page, at, `the correction's own end released the held gesture, so the arrival snapped (${log})`);
  await expectInBand(page, `premise: the arrival leaves the reader inside the band (${log})`);
}

/// Stop the recorder and give the clock back, whatever the test did.
async function teardown(page: Page) {
  await followRecorderStop(page);
  await page.clock.resume();
}

test.describe("An anchor correction's own scrollend", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a short arrival after the correction's end keeps a reader scrolling up inside the band (controlled order: move → correction → its end → arrival)", async ({
    page,
  }) => {
    try {
      const { at } = await correctedHeldGesture(page);
      await expectShortArrivalHeld(page, at);
    } finally {
      await teardown(page);
    }
  });

  test("the corrected gesture settles at the reader's quiet deadline, and inside the band the next arrival follows", async ({
    page,
  }) => {
    try {
      const { at } = await correctedHeldGesture(page);
      await expectShortArrivalHeld(page, at);
      await toQuietDeadline(page);
      await followDeliver(page, "arrival after the quiet deadline");
      await expectSettledAtBottom(
        page,
        `the corrected gesture did not settle at the reader's quiet deadline (${await followLog(page)})`,
      );
    } finally {
      await teardown(page);
    }
  });

  test("the corrected gesture settles at the reader's quiet deadline, and outside the band the reader stays parked", async ({
    page,
  }) => {
    try {
      const { at } = await correctedHeldGesture(page);
      await followDeliver(page, TALL("tall arrival after the correction"));
      await expectRowHeld(page, at, `the tall arrival moved the reader's message (${await followLog(page)})`);
      expect(
        await distanceFromBottom(page),
        "premise: the tall arrival leaves the reader outside the band",
      ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
      await toQuietDeadline(page);
      await followDeliver(page, `arrival after the quiet deadline: ${"r".repeat(200)}`);
      await expectRowHeld(page, at, `the settle outside the band did not leave the reader parked (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });

  // The native-mode counterpart of the debounce spec's late-move case: the
  // correction's end was refused, so the reader's quiet deadline is armed, and a
  // reader move the deadline finds made but not yet delivered must start a
  // fresh quiet interval. With `scrollend` nothing else re-arms it (the fired
  // handle is gone by then), so before, the deadline settled over the move. The
  // late move's own end is gated: it is a programmatic write, which every
  // engine ends at once, and would settle the gesture on its own.
  test("a reader move just before the correction's quiet deadline restarts it, counted from that move (controlled order: correction end → move → deadline → its scroll event)", async ({
    page,
  }) => {
    const LATE_PX = -6;
    try {
      await correctedHeldGesture(page, { observeSettle: true });
      const armed = await followSettleRecord(page);
      expect(armed.settle.length, `premise: the refused correction end armed one deadline (${JSON.stringify(armed)})`).toBe(1);
      const deadline = armed.settle[0];
      expect(deadline.cleared ?? deadline.fired, "premise: the deadline is pending").toBeNull();
      expect(deadline.delay, "premise: it runs from the reader's move, not the correction").toBe(QUIET_MS - READER_QUIET_MS);
      expect(await followGate(page), "premise: the gate was removed for the correction").toBe(false);
      await followMark(page, "late armed");
      await followBeforeSettleFire(page, LATE_PX);
      await toQuietDeadline(page);
      await followRealFrames(page, 3);

      const fired = await followSettleRecord(page);
      const log = await followLog(page);
      const what = `${JSON.stringify(fired)}; ${log}`;
      expect(fired.beforeFire.length, `premise: the late move ran ahead of the deadline (${what})`).toBe(1);
      const late = fired.beforeFire[0];
      expect(late.handle, `premise: ahead of the armed deadline (${what})`).toBe(deadline.handle);
      expect(late.after - late.before, `premise: the late move went up (${what})`).toBeCloseTo(LATE_PX, 0);
      expect(log.slice(log.lastIndexOf("late armed")), `premise: move, deadline, then its scroll event; its end held (${what})`).toMatch(
        /^late armed late move deadline scroll\b/,
      );
      expect(log.slice(log.lastIndexOf("late armed")), `premise: no end reached the app after the late move (${what})`).not.toMatch(
        /\bend\b/,
      );
      const at = await newestVisibleRow(page);
      expect(at, "premise: a message should be visible").not.toBeNull();
      expect(await distanceFromBottom(page), `premise: the reader is still inside the band (${what})`).toBeLessThanOrEqual(
        BOTTOM_THRESHOLD_PX - 40 - IN_PLACE_TOLERANCE_PX,
      );

      await followDeliver(page, "join");
      await expectRowHeld(page, at!, `the deadline settled over the late reader move, so the arrival snapped (${what})`);

      const after = await followSettleRecord(page);
      const pending = after.settle.filter((t) => t.cleared === null && t.fired === null);
      expect(pending.length, `a fresh deadline after the late move (${JSON.stringify(after)})`).toBe(1);
      expect(pending[0].at, "the fresh deadline was armed at the late move").toBe(late.at);
      expect(pending[0].delay, "the fresh deadline is a full quiet interval").toBe(QUIET_MS);
      await page.clock.runFor(late.at + QUIET_MS - 1 - after.now);
      await followRealFrames(page, 2);
      expect((await followSettleRecord(page)).settle.filter((t) => t.fired !== null).length, "the gesture settled early").toBe(1);
      expect(await rowDrift(page, at!), "the hold let go before the fresh deadline").toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      const beforeSettle = await scrollTop(page);
      await page.clock.runFor(1);
      await followRealFrames(page, 2);
      expect(await scrollTop(page), "the settle moved the view").toBe(beforeSettle);
      expect(
        (await followSettleRecord(page)).settle.filter((t) => t.fired !== null).map((t) => t.handle),
        "premise: the fresh deadline settled the gesture",
      ).toEqual([deadline.handle, pending[0].handle]);
      await followDeliver(page, "arrival after the fresh deadline");
      await expectSettledAtBottom(page, `the gesture settled inside the band was not followed (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });

  // The correction's own end is recognised by where it left the view. A
  // container resize between the correction and that end (the composer
  // growing) moves the view's bottom edge and not its top, and nobody moved the
  // view; before, the end no longer matched both edges, so it settled the held
  // gesture and the observer's restore then snapped the reader. Matching now
  // compares the top and the reader revision. The resize is made in a
  // capture-phase listener ahead of the app's, at the correction's real end, so
  // no observer can report it first.
  test("a container resize just before the correction's end keeps the gesture held (controlled order: correction → resize → its end → observer)", async ({
    page,
  }) => {
    const SHRINK_PX = 24;
    try {
      const at = await heldGesture(page);
      await page.clock.runFor(READER_QUIET_MS);
      expect(await followUngate(page), "premise: the gate was still in place").toBe(true);
      await followRealFrames(page, 3);
      await followResizeBeforeEnd(page, SHRINK_PX);
      const grow = await followGrowAbove(page, GROW_PX);
      expect(Math.abs(grow.after - grow.before), "premise: the growth did not clamp the view").toBeLessThanOrEqual(1);
      const ended = await followAwaitAfter(page, "grow", "end", 20);
      await followRealFrames(page, 3);
      const log = await followLog(page);
      const resize = await followResizeRecord(page);
      const what = `${JSON.stringify({ grow, resize })}; ${log}`;
      test.info().annotations.push({ type: "resize before the correction's end", description: what });
      expect(ended, `premise: the correction's own scrollend reached the app (${what})`).toBe(true);
      expect(resize.ran, `premise: the resize ran at the correction's end (${what})`).toBe(true);
      expect(
        log.slice(log.lastIndexOf("ungated")),
        `premise: correction, then the resize at its end, which reached the app before any observer (${what})`,
      ).toMatch(/^ungated (scroll )*grow (scroll )+resize end\b.*\bobserved\b/);
      expect(
        Math.abs(resize.before.top - grow.before - GROW_PX),
        `premise: the correction moved the view by the growth before its end (${what})`,
      ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      expect(resize.after.top, `premise: the resize left the top where the correction put it (${what})`).toBe(resize.before.top);
      expect(resize.before.height - resize.after.height, `premise: the container shrank (${what})`).toBe(SHRINK_PX);
      expect(await distanceFromBottom(page), `premise: the reader is still inside the band (${what})`).toBeLessThanOrEqual(
        BOTTOM_THRESHOLD_PX - 40 - IN_PLACE_TOLERANCE_PX,
      );

      expect(
        await rowDrift(page, at),
        `the resized correction's end settled the held gesture, and the observer's restore then snapped the reader (${what})`,
      ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      await expectShortArrivalHeld(page, at);
      await toQuietDeadline(page);
      await followDeliver(page, "arrival after the quiet deadline");
      await expectSettledAtBottom(
        page,
        `the resized gesture did not settle at the reader's quiet deadline (${await followLog(page)})`,
      );
    } finally {
      await teardown(page);
    }
  });

  test("a container-growth clamp before the correction's end keeps the gesture held (controlled order: correction → growth clamp → its end → observer)", async ({ page }) => {
    const GROW_CONTAINER_PX = 60;
    try {
      const at = await heldGesture(page, { initialShrinkPx: 80, observeSettle: true });
      await page.clock.runFor(READER_QUIET_MS);
      await followUngate(page);
      await followRealFrames(page, 3);
      await followResizeBeforeEnd(page, -GROW_CONTAINER_PX);
      const grow = await followGrowAbove(page, GROW_PX);
      expect(Math.abs(grow.after - grow.before), "premise: row growth did not clamp the top").toBeLessThanOrEqual(1);
      const ended = await followAwaitAfter(page, "grow", "end", 20);
      await followRealFrames(page, 3);
      const resize = await followResizeRecord(page);
      const log = await followLog(page);
      const what = `${JSON.stringify({ grow, resize })}; ${log}`;
      test.info().annotations.push({ type: "growth clamp before correction end", description: what });
      expect(ended, `premise: the correction's end reached the app (${what})`).toBe(true);
      expect(resize.ran).toBe(true);
      expect(log.slice(log.lastIndexOf("ungated")), `premise: the growth clamp precedes end and observer (${what})`).toMatch(/^ungated (scroll )*grow (scroll )+resize end\b.*\bobserved\b/);
      expect(Math.abs(resize.before.top - grow.before - GROW_PX), "premise: the anchor correction moved the top").toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      expect(resize.after.height - resize.before.height, "premise: the container grew").toBe(GROW_CONTAINER_PX);
      expect(resize.before.top - resize.after.top, "premise: container growth clamped scrollTop").toBeGreaterThan(SCROLL_TOP_SLACK_PX);
      expect(resize.after.top + resize.after.height - resize.before.top - resize.before.height, "premise: the bottom edge changed too").toBeGreaterThan(SCROLL_TOP_SLACK_PX);
      expect(await distanceFromBottom(page), "premise: the reader remains inside the follow band").toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX);
      expect(await rowDrift(page, at), `the growth clamp accepted the correction's own end and released the hold (${what})`).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      const armed = await followSettleRecord(page);
      const pending = armed.settle.filter((t) => t.cleared === null && t.fired === null);
      expect(pending, "the refused end arms one quiet deadline").toHaveLength(1);
      expect(pending[0].delay, "layout does not restart reader quiet time").toBe(QUIET_MS - READER_QUIET_MS);
      await expectShortArrivalHeld(page, at);
      await page.clock.runFor(QUIET_MS - READER_QUIET_MS - 1);
      expect(await rowDrift(page, at), "the hold lasts until the reader's deadline").toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      await page.clock.runFor(1);
      await followDeliver(page, "arrival after the growth clamp deadline");
      await expectSettledAtBottom(page, "legitimate quiet settlement resumes in-band following");
    } finally {
      await teardown(page);
      await page.evaluate(() => document.getElementById("chat-scroll-container")?.style.removeProperty("max-height"));
    }
  });

  // The same growth clamp with no correction before it: the reader has only
  // moved, so there is no evidence of ours for the clamp's end to match. The
  // container grows under a reader 20px above the end and the browser clamps
  // `scrollTop` to the new end; an engine can deliver that clamp's `scrollend`
  // before its `scroll` and before the observer. Before, the end found no
  // correction to refuse it by, settled the held gesture with the pin still set,
  // and the observer's restore then snapped the reader. A native end that takes
  // in a pending layout movement is now refused whatever the evidence, and the
  // reader's own quiet deadline settles the gesture. Only the controlled end
  // reaches the app: the reader's, the clamp's and the restore's native ends
  // stay gated, so none of them can release the hold being measured.
  test("a container-growth clamp's own end with no prior correction keeps the gesture held (controlled order: move → growth clamp → its end → scroll → observer)", async ({ page }) => {
    const GROW_CONTAINER_PX = 60;
    try {
      const at = await heldGesture(page, { initialShrinkPx: 80, observeSettle: true });
      const moved = await scrollTop(page);
      await page.clock.runFor(READER_QUIET_MS);
      await followRealFrames(page, 3);
      const run = await followGrowContainerThenEnd(page, GROW_CONTAINER_PX);
      await followRealFrames(page, 3);
      const log = await followLog(page);
      const what = `${JSON.stringify(run)}; ${log}`;
      test.info().annotations.push({ type: "growth clamp end without a correction", description: what });
      expect(
        log.slice(log.lastIndexOf("move")),
        `premise: one delivered reader move with its end gated, then the growth, with no scroll between (${what})`,
      ).toMatch(/^move scroll (held )+container grow\b/);
      expect(run.before.top, `premise: no layout correction moved the view after the reader (${what})`).toBe(moved);
      expect(run.before.max - run.before.top, `premise: the reader was inside the follow band (${what})`).toBeLessThanOrEqual(
        BOTTOM_THRESHOLD_PX - IN_PLACE_TOLERANCE_PX,
      );
      expect(run.after.height - run.before.height, `premise: the container grew (${what})`).toBe(GROW_CONTAINER_PX);
      expect(run.before.top - run.after.top, `premise: the growth clamped scrollTop (${what})`).toBeGreaterThan(SCROLL_TOP_SLACK_PX);
      expect(run.after.max - run.after.top, `premise: the clamp is to the new end (${what})`).toBeLessThanOrEqual(SCROLL_TOP_SLACK_PX);
      expect(
        run.after.top + run.after.height - run.before.top - run.before.height,
        `premise: the bottom edge moved too (${what})`,
      ).toBeGreaterThan(SCROLL_TOP_SLACK_PX);
      expect(run.scrollsAtEnd, `premise: the end reached the app before the clamp's scroll (${what})`).toBe(0);
      expect(
        log.slice(log.lastIndexOf("container grow")),
        `premise: the end, then the clamp's scroll, then the observer (${what})`,
      ).toMatch(/^container grow end (held )*scroll\b.*\bobserved\b/);

      expect(
        await rowDrift(page, at),
        `the growth clamp's own end settled the held gesture with no correction to refuse it, and the observer's restore then snapped the reader (${what})`,
      ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      const armed = await followSettleRecord(page);
      expect(armed.settle, `the refused end armed one quiet deadline (${JSON.stringify(armed)})`).toHaveLength(1);
      const deadline = armed.settle[0];
      expect(deadline.cleared ?? deadline.fired, "the deadline is pending").toBeNull();
      expect(deadline.delay, "it runs from the reader's move, not the clamp or its end").toBe(QUIET_MS - READER_QUIET_MS);

      await followDeliver(page, "join");
      await expectRowHeld(page, at, `a short arrival moved the held reader after the clamp's end (${await followLog(page)})`);
      await expectInBand(page, "premise: the short arrival leaves the reader inside the band");
      await page.clock.runFor(QUIET_MS - READER_QUIET_MS - 1);
      await followRealFrames(page, 2);
      expect((await followSettleRecord(page)).settle.filter((t) => t.fired !== null), "the gesture settled early").toEqual([]);
      expect(await rowDrift(page, at), "the hold lasts until the reader's deadline").toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      await page.clock.runFor(1);
      await followRealFrames(page, 2);
      expect(
        (await followSettleRecord(page)).settle.filter((t) => t.fired !== null).map((t) => t.handle),
        "premise: the reader's deadline settled the gesture",
      ).toEqual([deadline.handle]);
      await followDeliver(page, "arrival after the growth clamp's deadline");
      await expectSettledAtBottom(page, `quiet settlement did not resume in-band following (${await followLog(page)})`);
    } finally {
      await teardown(page);
      await page.evaluate(() => document.getElementById("chat-scroll-container")?.style.removeProperty("max-height"));
    }
  });

  test("a reader move after the correction settles on its own end, even back where the correction left the view", async ({
    page,
  }) => {
    const AWAY_PX = 10;
    try {
      const { correctedTop } = await correctedHeldGesture(page);
      expect(await followGate(page), "premise: the gate was removed for the correction").toBe(false);
      await followMark(page, "away");
      const away = await followReaderMove(page, -AWAY_PX);
      expect(away.delivered, "premise: the move away reached the app").toBe(true);
      expect(await followAwaitAfter(page, "away", "held"), "premise: the gate held the move away's end").toBe(true);
      await followRealFrames(page, 3);
      expect(await followUngate(page), "premise: the gate was in place").toBe(true);
      await followRealFrames(page, 3);
      await followMark(page, "back");
      const back = await followReaderMove(page, AWAY_PX);
      expect(back.delivered, "premise: the move back reached the app").toBe(true);
      expect(back.after, "premise: the reader is back exactly where the correction left the view").toBe(correctedTop);
      const ended = await followAwaitAfter(page, "back", "end", 20);
      const log = await followLog(page);
      expect(ended, `premise: the move back's own end reached the app (${log})`).toBe(true);
      expect(log.slice(log.lastIndexOf("ungated")), `premise: nothing ended before the move back (${log})`).toMatch(
        /^ungated (scroll )*back\b/,
      );
      await followDeliver(page, "join");
      await expectSettledAtBottom(
        page,
        `the old correction's match refused the reader's own end, so the gesture stayed held (${log})`,
      );
    } finally {
      await teardown(page);
    }
  });

  test("a room switch with the correction's deadline pending leaves the new room's gesture its own", async ({
    page,
  }) => {
    try {
      await correctedHeldGesture(page, { path: DEEP_ROOM_PATH });
      await callRiverTest(page, "switchRoom", "Deep History Room");
      await page.clock.runFor(0);
      await expect(page.getByRole("heading", { name: "Deep History Room" })).toBeVisible();
      await followRealFrames(page, 3);
      await page.clock.runFor(5);
      await followRealFrames(page, 2);
      expect(await distanceFromBottom(page), "premise: the new room opened at its newest message").toBeLessThanOrEqual(
        AT_BOTTOM_EPSILON_PX,
      );
      // The newest message can be from yesterday in the browser's timezone; let
      // one arrival bring the "Today" divider in first.
      await followDeliver(page, "first arrival in the new room");
      await expectSettledAtBottom(page, "premise: the new room follows before the gesture");

      expect(await followGate(page), "premise: the gate was removed for the correction").toBe(false);
      await followMark(page, "new room move");
      const move = await followReaderMove(page, -CORRECTED_UP_PX);
      expect(move.delivered, "premise: the new room's scroll event reached the app").toBe(true);
      expect(await followAwaitAfter(page, "new room move", "held"), "premise: the gate held its end").toBe(true);
      await followRealFrames(page, 3);
      const at = await newestVisibleRow(page);
      expect(at, "premise: a message should be visible").not.toBeNull();
      // Past the old deadline, and a full quiet interval after the new move.
      await page.clock.runFor(QUIET_MS + 1);
      await followDeliver(page, "join");
      await expectRowHeld(
        page,
        at!,
        `work left from the old room's correction settled the new room's gesture (${await followLog(page)})`,
      );
    } finally {
      await teardown(page);
    }
  });

  test("a new seek with the correction's deadline pending reaches the newest message, then follows", async ({
    page,
  }) => {
    try {
      await correctedHeldGesture(page, { upPx: 1_200, fillers: 25 });
      await page.clock.runFor(0);
      await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({ timeout: 5_000 });
      await page.getByTestId("scroll-to-bottom").click();
      // Past the old deadline on the way: the seek is not a gesture it could end.
      for (let i = 0; i < 60 && (await distanceFromBottom(page)) > AT_BOTTOM_EPSILON_PX; i++) await followFrame(page);
      await expectSettledAtBottom(page, `the seek did not reach the newest message (${await followLog(page)})`);
      await followDeliver(page, "arrival after the seek");
      await expectSettledAtBottom(page, "the follow did not survive the seek");
    } finally {
      await teardown(page);
    }
  });
  // Native input, no gate, on the browser's own clock: a held finger on
  // Chromium (it sends no end until it lifts) or small wheel ticks elsewhere,
  // then growth above the reader and, once the correction's end has reached the
  // app or 20 frames have gone by, a short arrival. Whatever order the engine
  // chose, the outcome has to match it. This says nothing about how long a real
  // wheel gesture lives; the controlled cases above own the behaviour.
  test("native input probe: a gesture, growth above it, then a short arrival", async ({
    page,
    browserName,
    isMobile,
  }) => {
    test.skip(browserName === "webkit" && isMobile, "mobile WebKit has no gesture input that stays unsettled between frames");
    await openRoomAtBottom(page, "Team Chat Room");
    for (let i = 0; i < 8; i++) await deliver(page, `filler ${i}: ${"y".repeat(200)}`);
    await expectSettledAtBottom(page, "premise: the fillers should have been followed");
    await afterLayoutSettles(page);
    const box = (await page.locator("#chat-scroll-container").boundingBox())!;
    const x = box.x + box.width / 2;
    let y = box.y + box.height / 2;
    const cdp = browserName === "chromium" ? await page.context().newCDPSession(page) : null;
    const touch = (type: string) =>
      cdp!.send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x, y }] });
    await followRecorderStart(page, { ownClock: true });
    await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      const probe = { lastScroll: 0, live: true };
      c.addEventListener("scroll", () => probe.live && (probe.lastScroll = Date.now()));
      (window as unknown as { __followProbe: typeof probe }).__followProbe = probe;
    });
    try {
      const start = await scrollTop(page);
      const up = async () => start - (await scrollTop(page));
      if (cdp) {
        await touch("touchStart");
        for (let i = 0; i < 40 && (await up()) <= 0; i++) {
          y += 1;
          await touch("touchMove");
          await page.waitForTimeout(20);
        }
        for (let i = 0; i < 60 && (await up()) < CORRECTED_UP_PX; i++) {
          y += 1;
          await touch("touchMove");
          await page.waitForTimeout(20);
        }
      } else {
        await page.mouse.move(x, y);
        for (let i = 0; i < 20 && (await up()) < CORRECTED_UP_PX; i++) {
          await page.mouse.wheel(0, -5);
          await page.waitForTimeout(20);
        }
      }
      expect(await up(), "premise: the gesture moved the view up past rounding").toBeGreaterThan(SCROLL_TOP_SLACK_PX);
      await followMark(page, "moved");
      const run = await page.evaluate((grow) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__followRecorder!;
        const probe = (window as unknown as { __followProbe: { lastScroll: number; live: boolean } }).__followProbe;
        const box = c.getBoundingClientRect();
        let at: { id: string; gap: number } | null = null;
        for (const row of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
          const r = row.getBoundingClientRect();
          if (r.bottom > box.top && r.top < box.bottom) at = { id: row.id, gap: box.bottom - r.top };
        }
        const above = Array.from(c.querySelectorAll<HTMLElement>('[id^="msg-"]'))
          .filter((r) => r.getBoundingClientRect().bottom < box.top)
          .at(-1)!;
        const endedBefore = rec.log.slice(rec.log.lastIndexOf("moved")).includes("end");
        probe.live = false;
        const lastReader = probe.lastScroll;
        const before = c.scrollTop;
        above.style.paddingTop = `${grow}px`;
        rec.log.push("grow");
        const grewAt = Date.now();
        let endAt: number | null = null;
        const onEnd = () => (endAt ??= Date.now());
        c.addEventListener("scrollend", onEnd);
        const events = () => document.querySelectorAll("#chat-content [data-anchor-row][data-item-key]").length;
        return new Promise<{
          at: typeof at;
          endedBefore: boolean;
          lastReader: number;
          grewAt: number;
          endAt: number | null;
          patchAt: number;
          correction: number;
        }>((resolve) => {
          let frames = 0;
          const deliverNow = () => {
            c.removeEventListener("scrollend", onEnd);
            const correction = c.scrollTop - before;
            const count = events();
            rec.log.push("deliver");
            window.__riverTest!.appendJoinEvent();
            const observer = new MutationObserver(() => {
              if (events() <= count) return;
              observer.disconnect();
              const patchAt = Date.now();
              rec.log.push("patch");
              rec.afterRealFrame(() =>
                rec.afterRealFrame(() => resolve({ at, endedBefore, lastReader, grewAt, endAt, patchAt, correction })),
              );
            });
            observer.observe(document.getElementById("chat-content")!, { childList: true, subtree: true });
          };
          const wait = () => rec.afterRealFrame(() => (endAt !== null || ++frames >= 20 ? deliverNow() : wait()));
          wait();
        });
      }, GROW_PX);
      await afterLayoutSettles(page);
      const log = await followLog(page);
      const drift = await rowDrift(page, run.at!);
      const distance = await distanceFromBottom(page);
      const t = (v: number | null) => (v === null ? "none" : `${v - run.lastReader}ms`);
      const timeline =
        `${log}; after the reader's last scroll: growth ${t(run.grewAt)}, correction end ${t(run.endAt)}, ` +
        `patch ${t(run.patchAt)}; correction ${run.correction}px, then drift ${drift.toFixed(1)}px, ${distance.toFixed(1)}px above the end`;
      test.info().annotations.push({ type: "native order", description: timeline });
      expect(run.at, "premise: a message should be visible").not.toBeNull();
      const held = drift <= IN_PLACE_TOLERANCE_PX;
      const followed = distance <= AT_BOTTOM_EPSILON_PX;
      expect(held || followed, `the arrival neither held the reader's message nor followed (${timeline})`).toBe(true);
      // Settled before the growth: a pinned reader is snapped by it. Still held
      // with no end at all: nothing settles it. Otherwise the quiet deadline
      // decides, with 30ms either side of it left to the engine.
      const quiet = run.patchAt - run.lastReader;
      const expected = run.endedBefore
        ? "followed"
        : run.endAt === null || quiet < QUIET_MS - 30
          ? "held"
          : quiet > QUIET_MS + 30
            ? "followed"
            : null;
      if (expected) expect(held ? "held" : "followed", `the outcome does not match the order (${timeline})`).toBe(expected);
    } finally {
      if (cdp) await touch("touchEnd").catch(() => {});
      await followRecorderStop(page);
    }
  });
});

test.describe("An anchor correction's own scrollend, on the mobile layout", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("a hide with the correction's deadline pending settles on the reveal, and the next gesture is its own", async ({
    page,
  }) => {
    const chat = page.locator("#chat-scroll-container");
    try {
      await correctedHeldGesture(page);
      await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
      await page.clock.runFor(0);
      await expect(chat).toBeHidden({ timeout: 5_000 });
      await expect.poll(() => viewportHeight(page), { message: "premise: the hidden chat has no height" }).toBe(0);
      await followRealFrames(page, 2);
      // The deadline runs while the chat is hidden: the settle waits for the reveal.
      await page.clock.runFor(QUIET_MS);
      await followRealFrames(page, 2);
      await page.getByTestId("rooms-back-button").click();
      await page.clock.runFor(0);
      await expect(chat).toBeVisible();
      await followRealFrames(page, 3);
      await followDeliver(page, "join");
      await expectSettledAtBottom(page, `the gesture settled inside the band was not followed after the reveal (${await followLog(page)})`);

      // A new gesture holds its own arrival: nothing of the old one is left to end it.
      expect(await followGate(page), "premise: the gate was removed for the correction").toBe(false);
      await followMark(page, "next move");
      const move = await followReaderMove(page, -CORRECTED_UP_PX);
      expect(move.delivered, "premise: the next gesture's scroll event reached the app").toBe(true);
      expect(await followAwaitAfter(page, "next move", "held"), "premise: the gate held its end").toBe(true);
      await followRealFrames(page, 3);
      const at = await newestVisibleRow(page);
      expect(at, "premise: a message should be visible").not.toBeNull();
      await followDeliver(page, `arrival in the next gesture: ${"n".repeat(20)}`);
      await expectRowHeld(page, at!, `the next gesture did not hold its arrival (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });

  // The complement of the case above: the reveal comes back BEFORE the
  // correction's deadline. Its restore completes the gesture the hide ended, and
  // that completion now cancels the deadline. Before, the handle survived it, and
  // the next gesture's first move took the stale handle for a deadline of its
  // own and re-armed it (with `scrollend`, a reader move re-arms only a pending
  // one), so a gesture still held under a finger settled 120ms later and the
  // next short arrival snapped it. Ends are gated from the hide on, so the only
  // thing that can complete the old gesture is the reveal's restore, and the
  // new gesture's own end stands in for a finger still down.
  test("a reveal before the correction's deadline ends that deadline with the gesture, so the next gesture stays held past 120ms", async ({
    page,
  }) => {
    const chat = page.locator("#chat-scroll-container");
    try {
      await correctedHeldGesture(page, { observeSettle: true });
      const armed = await followSettleRecord(page);
      expect(armed.settle.length, `premise: the refused correction end armed one deadline (${JSON.stringify(armed)})`).toBe(1);
      const old = armed.settle[0];
      expect(old.cleared ?? old.fired, "premise: the correction's deadline is pending").toBeNull();
      expect(await followGate(page), "premise: the gate was removed for the correction").toBe(false);

      await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
      await page.clock.runFor(0);
      await expect(chat).toBeHidden({ timeout: 5_000 });
      await expect.poll(() => viewportHeight(page), { message: "premise: the hidden chat has no height" }).toBe(0);
      await followRealFrames(page, 2);
      await page.getByTestId("rooms-back-button").click();
      await page.clock.runFor(0);
      await expect(chat).toBeVisible();
      await followRealFrames(page, 3);
      const revealed = await followSettleRecord(page);
      const oldAtReveal = revealed.settle.find((t) => t.handle === old.handle)!;
      expect(revealed.now, "premise: the reveal came back before the old deadline was due").toBeLessThan(old.at + old.delay);
      expect(oldAtReveal.fired, "premise: the old deadline has not fired").toBeNull();
      await followDeliver(page, "arrival after the reveal");
      await expectSettledAtBottom(page, `the gesture the reveal completed inside the band was not followed (${await followLog(page)})`);

      await followMark(page, "next move");
      const move = await followReaderMove(page, -CORRECTED_UP_PX);
      expect(move.delivered, "premise: the next gesture's scroll event reached the app").toBe(true);
      expect(await followAwaitAfter(page, "next move", "held"), "premise: the gate held its end").toBe(true);
      await followRealFrames(page, 3);
      const at = await newestVisibleRow(page);
      expect(at, "premise: a message should be visible").not.toBeNull();
      expect(await distanceFromBottom(page), "premise: the next gesture is inside the band").toBeLessThanOrEqual(
        BOTTOM_THRESHOLD_PX - 40 - IN_PLACE_TOLERANCE_PX,
      );
      // Past a full quiet interval from the move, with the gesture still held.
      await page.clock.runFor(QUIET_MS + 1);
      await followRealFrames(page, 2);
      await followDeliver(page, "join");
      const what = `${JSON.stringify(await followSettleRecord(page))}; ${await followLog(page)}`;
      await expectRowHeld(
        page,
        at!,
        `the reveal left the old deadline attached, the next gesture re-armed it and settled at 120ms, so the arrival snapped (${what})`,
      );
      // Supporting record. Desktop WebKit's own reveal `scroll` can re-arm the
      // old deadline before the restore completes the gesture; the completion
      // then cancels whichever handle is pending.
      expect(oldAtReveal.cleared, `the old deadline was not cancelled by the reveal (${what})`).not.toBeNull();
      expect(
        revealed.settle.filter((t) => t.cleared === null),
        `a deadline was still pending after the reveal completed the gesture (${what})`,
      ).toEqual([]);
      expect(
        (await followSettleRecord(page)).settle.length,
        `the next gesture armed a deadline (${what})`,
      ).toBe(revealed.settle.length);

      // Its end, which the gate consumed, stands in as a synthetic one: the
      // gesture settles inside the band and follows again.
      expect(await followUngate(page), "premise: the gate was in place").toBe(true);
      await followMark(page, "release");
      await page.evaluate(() => document.getElementById("chat-scroll-container")!.dispatchEvent(new Event("scrollend")));
      expect(await followAwaitAfter(page, "release", "end"), "premise: the released end reached the app").toBe(true);
      await followDeliver(page, "arrival after the released end");
      await expectSettledAtBottom(page, `the next gesture's own end did not restore following (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });
});

// Every row the reader's anchor remembers is removed while their gesture is
// still open (a moderator deleting a run of messages), and the removal takes
// away more than they were parked above the end, so the browser clamps them into
// the follow band. A restore then finds no saved row to put back, and leaves
// the view where layout put it. Before, that restore did not count as one the
// gesture could not reach: the settle, or the reveal that finishes a gesture the
// hide cut short, measured the clamped position as the reader's own, re-pinned a
// reader who was outside the band, and the next arrival snapped them to it. A
// missing anchor now preserves the reader's intent as a constrained restore
// does: the gesture ends without measuring, the pin stays unset, and the reader
// stays parked until they scroll again.
//
// Arrivals are inbound and tall: from the clamped end, one that is followed
// moves `scrollTop` by its height, and one that is not leaves the reader outside
// the band. The removal is the app's `removeMessages` hook
// (history-missing-anchor-fixture.ts).

/// Up out of the band: the reader is parked, not following.
const MISSING_UP_PX = 300;
/// Enough filler messages that the removed neighbourhood is fillers only and
/// history is left on both sides of it.
const MISSING_FILLERS = 30;
/// Far taller than the follow band on its own.
const TALLER = (marker: string) => `${marker}\n${Array.from({ length: 30 }, (_, i) => `line ${i}`).join("\n")}`;

/// Two tall arrivals, the second to catch a snap that comes late: each must
/// leave `scrollTop` at the post-removal offset and the reader outside the band.
async function expectParkedThroughArrivals(page: Page, top: number, why: string) {
  for (const n of [1, 2]) {
    const marker = `tall arrival ${n} after the anchor rows were removed`;
    await followDeliver(page, TALLER(marker));
    const height = await page.getByText(marker, { exact: false }).last().evaluate((el) => el.getBoundingClientRect().height);
    expect(height, "premise: the arrival is taller than the follow band").toBeGreaterThan(BOTTOM_THRESHOLD_PX + IN_PLACE_TOLERANCE_PX);
    const drift = await missingAnchorDrift(page, top);
    expect(drift, `${why}: arrival ${n} moved the parked view by ${drift}px (${await followLog(page)})`).toBeLessThanOrEqual(
      IN_PLACE_TOLERANCE_PX,
    );
    expect(await distanceFromBottom(page), `${why}: arrival ${n} left the reader inside the band`).toBeGreaterThan(
      BOTTOM_THRESHOLD_PX,
    );
  }
}

test.describe("Every saved anchor row removed during a held gesture", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // The settle runs with nothing pending: the clamp's `scroll` and the observer
  // have both reached the app and the restore has recorded the clamped view, so
  // this is not the pending-clamp path that preserves on its own.
  test("a native end with nothing pending leaves the clamped reader parked (controlled order: move → removal → clamp scroll → observer → end)", async ({
    page,
  }) => {
    try {
      await heldGesture(page, { upPx: MISSING_UP_PX, fillers: MISSING_FILLERS });
      const selection = await missingAnchorSelect(page);
      await followMark(page, "remove");
      const removal = await missingAnchorRemove(page, selection, () => followRealFrames(page, 3));
      const ended = await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__followRecorder!;
        const top = c.scrollTop;
        const gated = rec.setGate(false);
        rec.log.push("settle");
        c.dispatchEvent(new Event("scrollend"));
        rec.setGate(gated);
        return { top, after: c.scrollTop, log: rec.log.slice(rec.log.lastIndexOf("remove")).join(" ") };
      });
      const what = `${JSON.stringify({ selection, removal, ended })}`;
      test.info().annotations.push({ type: "missing anchor native end", description: what });
      expect(ended.log, `premise: the clamp's scroll reached the app, its own end was gated, and only then the settle (${what})`).toMatch(
        /^remove .*\bscroll\b.* settle end$/,
      );
      expect(ended.log, `premise: no end reached the app before the settle (${what})`).not.toMatch(/\bend\b.* settle/);
      expect(ended.top, `premise: the view had not moved since the clamp's delivered scroll (${what})`).toBe(removal.topAtLastScroll);
      expect(ended.top, `premise: the settle found the view where the removal left it (${what})`).toBe(removal.after.top);
      expect(ended.after, `premise: the settle did not move the view (${what})`).toBe(ended.top);

      await followRealFrames(page, 2);
      await expectParkedThroughArrivals(page, removal.after.top, "the settle measured the clamp as the reader's and re-pinned them");

      // Preservation lasts until the reader moves: a real move back to the new
      // end is theirs, and following resumes from it.
      await followMark(page, "back to the end");
      const back = await followReaderMove(page, (await endMinus(page, 0)) - (await scrollTop(page)));
      expect(back.delivered, "premise: the reader's move back to the end reached the app").toBe(true);
      expect(back.max - back.after, "premise: the reader reached the end").toBeLessThanOrEqual(SCROLL_TOP_SLACK_PX);
      await followRealFrames(page, 2);
      await followDeliver(page, TALLER("arrival after the reader came back to the end"));
      await expectSettledAtBottom(page, `the reader's own move back to the end did not resume following (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });
});

test.describe("Every saved anchor row removed during a held gesture, on the mobile layout", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  for (const deadlinePassed of [false, true]) {
    test(`the reveal's restore finds no saved row and leaves the clamped reader parked, ${deadlinePassed ? "after" : "before"} the old quiet deadline`, async ({
      page,
    }) => {
      const chat = page.locator("#chat-scroll-container");
      await missingAnchorHideScrollbar(page);
      try {
        await heldGesture(page, { upPx: MISSING_UP_PX, fillers: MISSING_FILLERS, observeSettle: true });
        const selection = await missingAnchorSelect(page);
        // Arm the reader's quiet deadline as a held gesture can: a real anchor
        // correction, then its end refused.
        await followGrowAbove(page, GROW_PX);
        await followRealFrames(page, 3);
        await page.evaluate(() => {
          const rec = window.__followRecorder!;
          rec.setGate(false);
          document.getElementById("chat-scroll-container")!.dispatchEvent(new Event("scrollend"));
          rec.setGate(true);
        });
        const armed = await followSettleRecord(page);
        expect(
          armed.settle.filter((t) => t.cleared === null && t.fired === null),
          `premise: refusing the correction's end armed the reader's deadline (${JSON.stringify(armed)})`,
        ).toHaveLength(1);
        const deadline = armed.settle[0];

        const removal = await missingAnchorRemove(page, selection, () => followRealFrames(page, 3));
        await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
        await page.clock.runFor(0);
        await expect(chat).toBeHidden({ timeout: 5_000 });
        expect(await viewportHeight(page), "premise: hidden geometry is not measured").toBe(0);
        await followRealFrames(page, 2);
        if (deadlinePassed) await page.clock.runFor(QUIET_MS);
        await page.getByTestId("rooms-back-button").click();
        // Nested zero-delay work can be scheduled one clock millisecond later.
        await expect
          .poll(
            async () => {
              await page.clock.runFor(1);
              return viewportHeight(page);
            },
            { message: "premise: the back button reveals the history" },
          )
          .toBeGreaterThan(0);
        await expect(chat).toBeVisible();
        await followRealFrames(page, 3);
        const settled = await followSettleRecord(page);
        const revealed = await page.evaluate(() => {
          const c = document.getElementById("chat-scroll-container")!;
          return { top: c.scrollTop, height: c.clientHeight, max: c.scrollHeight - c.clientHeight };
        });
        const what = JSON.stringify({ selection, removal, revealed, settled, log: await followLog(page) });
        test.info().annotations.push({ type: "missing anchor reveal", description: what });
        const old = settled.settle.find((t) => t.handle === deadline.handle);
        expect(old, `premise: the old deadline is in the record (${what})`).toBeDefined();
        if (deadlinePassed) {
          expect(old!.fired, `premise: the old deadline fired while hidden (${what})`).not.toBeNull();
        } else {
          expect(settled.now, `premise: the reveal finished before the old deadline was due (${what})`).toBeLessThan(
            deadline.at + deadline.delay,
          );
          expect(old!.fired, `premise: the old deadline has not fired (${what})`).toBeNull();
        }
        expect(
          settled.settle.filter((t) => t.cleared === null && t.fired === null),
          `the reveal did not complete the old gesture and cancel its deadline (${what})`,
        ).toEqual([]);
        // The reveal writes nothing for a missing anchor, so the view is where
        // the removal left it, inside the band: measuring it as the reader's
        // would pin them.
        expect(
          Math.abs(revealed.top - removal.after.top),
          `premise: the reveal found no saved row to restore and left the view where the removal put it (${what})`,
        ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
        expect(
          revealed.max - revealed.top,
          `premise: the revealed view is inside the follow band, so measuring it would pin the reader (${what})`,
        ).toBeLessThanOrEqual(BOTTOM_THRESHOLD_PX - IN_PLACE_TOLERANCE_PX);

        await expectParkedThroughArrivals(page, removal.after.top, "the reveal measured the clamp as the reader's and re-pinned them");
      } finally {
        await teardown(page);
      }
    });
  }
});

// Content can grow above a held reader with `scrollTop` unchanged (an image
// loading; the container sets `overflow-anchor: none`), so no `scroll` event is
// coming, only the ResizeObserver's report. A settle that reaches the app first
// used to measure the reflowed rows as the reader's position: their message
// was saved where the growth had pushed it, and nothing put it back. The settle
// now restores the anchor first, then measures.
//
// The order is made, not waited for: the growth and a synthetic `scrollend` in
// one task, so the settle runs before any observer can deliver. It says nothing
// about whether an engine produces that order natively; the probe below asks.
test.describe("A settle before the observer reports a reflow above the reader", () => {
  test.use({ viewport: { width: 1280, height: 900 } });
  /// Far enough up that the reader is parked outside the band.
  const PARKED_UP_PX = 200;

  /// A held gesture `upPx` up, the gate removed, then the growth and the settle
  /// in one task. Checks the order and that the growth moved the reader's
  /// message before the app saw anything. Leaves the clock paused and the
  /// recorder running.
  async function settleBeforeObserver(page: Page, upPx: number) {
    const at = await heldGesture(page, { upPx });
    expect(await followUngate(page), "premise: the gate was still in place").toBe(true);
    await followRealFrames(page, 3);
    const run = await followGrowAboveThenEnd(page, GROW_PX, at.id);
    const log = await followLog(page);
    const what =
      `${at.id} at ${at.gap.toFixed(1)}px, moved ${run.shift.toFixed(1)}px by the growth; ` +
      `scrollTop ${run.before} → ${run.grown} grown → ${run.ended} after the settle; ${log}`;
    test.info().annotations.push({ type: "ordered settle", description: what });
    const beforeGrow = log.slice(log.lastIndexOf("move"), log.lastIndexOf("grow"));
    expect(beforeGrow, `premise: no end reached the app between the move and the growth (${what})`).not.toMatch(
      /\bend\b/,
    );
    expect(log.slice(log.lastIndexOf("grow")), `premise: the settle reached the app before the observer (${what})`).toMatch(
      /^grow end\b.*\bobserved\b/,
    );
    expect(Math.abs(run.grown - run.before), `premise: the growth did not move the view (${what})`).toBeLessThanOrEqual(1);
    expect(
      Math.abs(run.shift - GROW_PX),
      `premise: the growth pushed the reader's message down before the settle (${what})`,
    ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
    return { at, what };
  }

  test("a reader parked outside the band keeps their message (controlled order: growth → settle → observer)", async ({
    page,
  }) => {
    try {
      const { at, what } = await settleBeforeObserver(page, PARKED_UP_PX);
      await followRealFrames(page, 3);
      expect(
        await rowDrift(page, at),
        `the settle measured the reflowed rows before putting the anchor back, so the reader's message moved (${what})`,
      ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
      expect(await distanceFromBottom(page), `premise: the reader is outside the band (${what})`).toBeGreaterThan(
        BOTTOM_THRESHOLD_PX,
      );
      await followDeliver(page, `arrival after the reflow: ${"r".repeat(200)}`);
      await expectRowHeld(page, at, `an arrival after the settle moved the reader's message (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });

  test("a reader inside the band is still following (controlled order: growth → settle → observer)", async ({
    page,
  }) => {
    try {
      const { what } = await settleBeforeObserver(page, CORRECTED_UP_PX);
      await followRealFrames(page, 3);
      // Settled inside the band, so following: the observer's restore snaps.
      expect(
        await distanceFromBottom(page),
        `the settle measured the pin in the reflowed view, so the reader stopped following (${what})`,
      ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
      await followDeliver(page, "join");
      await expectSettledAtBottom(page, `the next arrival was not followed (${await followLog(page)})`);
    } finally {
      await teardown(page);
    }
  });

  // Native, ungated, on the browser's own clock: three wheel ticks up, then a
  // real image above the reader finishes loading, its response sent each delay
  // in PROBE_DELAYS_MS after the last tick. The page records every `scroll` and
  // `scrollend` once the app has had it, the image's `load`, and every
  // observer delivery, each with the image row's height. The order in question
  // is a settle that first sees the grown row, before any observer has
  // reported it, with no settle since the reader's last scroll. Whatever order
  // the engine chose, the reader's message has to stay where their last scroll
  // left it. Ordering evidence only: it says nothing about real wheel lifetime.
  //
  // Measured here (2026-10-02, headless): Chromium ends every wheel tick at
  // once, and Firefox sent no end of its own before the correction's; WebKit
  // ends the wheel ~100ms after its last scroll, in the same rendering pass as
  // the observer and before it, so an image loading 75-85ms after the ticks
  // produced the order natively (and, before the fix, lost the message). The
  // delays straddle that window; elsewhere they only check the outcome.
  test("native input probe: wheel ticks, then an image loading above the reader", async ({ page, isMobile }) => {
    test.skip(isMobile, "no wheel input on the mobile projects (see the wheel smoke above)");
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
        await expectSettledAtBottom(page, "premise: the fillers should have been followed");
        await afterLayoutSettles(page);
        const box = (await page.locator("#chat-scroll-container").boundingBox())!;
        await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await page.evaluate((url) => {
          const c = document.getElementById("chat-scroll-container")!;
          const img = c.querySelector<HTMLImageElement>(`img[src$="${url}"]`)!;
          const row = img.closest<HTMLElement>('[id^="msg-"]')!;
          const events: ProbeEvent[] = [];
          const newest = () => {
            const view = c.getBoundingClientRect();
            let at: { id: string; gap: number } | null = null;
            for (const r of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
              const b = r.getBoundingClientRect();
              if (b.bottom > view.top && b.top < view.bottom) at = { id: r.id, gap: view.bottom - b.top };
            }
            return at;
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
        const disputed =
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
          `settle before observer: ${disputed ? "OBSERVED" : "not observed"}${endedBefore ? " (settled before the growth)" : ""}; ` +
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
