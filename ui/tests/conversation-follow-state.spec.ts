import { test, expect, Page } from "@playwright/test";
import { seekClockInstall, seekClockPause } from "./history-scroll-fixture";
import {
  FollowEntry,
  followFrame,
  followReaderMove,
  followRecorderStart,
  followRecorderStop,
  followTimeline,
  newestVisibleRow,
  rowDrift,
} from "./history-follow-fixture";
import {
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  endMinus,
  expectSettledAtBottom,
  openRoomAtBottom,
  readerScrollsWithoutGesture,
  scrollTop,
} from "./history-scroll-helpers";

// How the history's follow state moves between Free, Gesture and Seeking when
// the reader and our own work interleave (history_scroll.rs, "Follow states").
//
// Assumes the example-data build (`window.__riverTest`). Arrivals are INBOUND:
// sending through the composer forces a snap and would prove nothing.

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
const BOTTOM_THRESHOLD_PX = 100;
/// Matches SCROLL_TOP_SLACK_PX there: the most one move may be and still be
/// rounding. Only a premise here, so a policy change fails at setup.
const SCROLL_TOP_SLACK_PX = 2;
/// The geometry budget for "the reader's message did not move" (as in
/// conversation-autoscroll.spec.ts), not derived from the slack above.
const IN_PLACE_TOLERANCE_PX = 4;

/// Fill Team Chat Room with tall rows and park the reader `px` above the end.
async function parkAboveTheEnd(page: Page, px: number) {
  await openRoomAtBottom(page, "Team Chat Room");
  await page.evaluate(async () => {
    for (let i = 0; i < 30; i++) {
      window.__riverTest!.appendMessage(`follow filler ${i}: ${"w ".repeat(450)}`);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    }
  });
  await expect(page.getByText("follow filler 29:")).toBeAttached({ timeout: 5_000 });
  await expectSettledAtBottom(page, "premise: the fillers should have been followed");
  await readerScrollsWithoutGesture(page, await endMinus(page, px));
  await afterLayoutSettles(page);
  expect(await distanceFromBottom(page), "premise: the reader should be parked far up").toBeGreaterThan(
    px - BOTTOM_THRESHOLD_PX,
  );
  await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({ timeout: 5_000 });
}

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
