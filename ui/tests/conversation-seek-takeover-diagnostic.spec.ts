import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { newestVisibleRow, rowDrift } from "./history-follow-fixture";
import {
  BOTTOM_THRESHOLD_PX,
  afterLayoutSettles,
  distanceFromBottom,
  parkAboveTheEnd,
  scrollTop,
} from "./history-scroll-helpers";

// OPT-IN DIAGNOSTIC: not part of the default run. playwright.config.ts ignores
// this file unless RIVER_SCROLL_DIAGNOSTICS=1:
//
//   RIVER_SCROLL_DIAGNOSTICS=1 npx playwright test conversation-seek-takeover-diagnostic.spec.ts
//
// It is exploratory, not a regression test. It drives native input against the
// scroll-to-latest animation, publishes every attempt's timeline as an
// annotation, and only asserts an outcome when a held finger lands mid-flight
// inside the band, which the measurement below never saw. A green run can
// therefore be observation alone. The deterministic takeover regressions and
// the native-input smoke test stay in conversation-follow-state.spec.ts.
//
// Moving it here does not close the gap it probes: the stale seek-end ordering
// documented in ui/src/components/conversation/history_scroll.rs ("Stale ends")
// is still unproven either way.

/// Matches SCROLL_TOP_SLACK_PX in ui/src/components/conversation.rs.
const SCROLL_TOP_SLACK_PX = 2;
/// The geometry budget for "the reader's message did not move", as in
/// conversation-follow-state.spec.ts.
const IN_PLACE_TOLERANCE_PX = 4;

/// One line of the seek takeover diagnostic's log.
type SeekEndEvent = { kind: "scroll" | "end" | "wheel" | "touch" | "deliver"; t: number; top: number; left: number };

// Native input, ungated, on the browser's own clock: the scroll-to-latest
// animation, and the reader taking over as it nears the band. Each of the
// animation's frames is a `scrollTop` write, which an engine ends on its own;
// the question is whether a frame's end can reach the app AFTER the reader's
// first upward move, where it would read as the gesture's end and settle it.
// Chromium (desktop and mobile) uses a held CDP finger, which sends no end of
// its own until it lifts, so any end after the upward move is not the reader's
// and a short arrival then shows whether it released the gesture. Elsewhere,
// wheel ticks: their own ends are indistinguishable from a frame's, so the
// order is recorded and nothing about the outcome is claimed. The animation is
// started from the same distance each time and the input sent once it is within
// each trigger; whether it lands mid-flight is the engine's and the host's
// business, and every attempt is reported.
//
// Measured here (2026-10-02, headless, macOS): no engine produced the order. Every
// frame's end reached the app in the same millisecond as its own `scroll`,
// before the next event. Chromium's last frame can end after the `touchstart` that
// stopped the animation, but before any upward move, when the follow is `Free`
// and a settle does nothing. The held finger landed 120-290px from the end
// (protocol latency), never inside the band, so its outcome check never ran.
// Firefox's wheel ticks never took over mid-flight: the next frame's write
// cancels its smooth wheel scroll. WebKit's took over and ended ~100ms after the
// last tick, its own end.
test.describe("A seek frame's end after the reader takes over", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("native input diagnostic: a takeover near the band, then any end that follows it", async ({
    page,
    browserName,
    isMobile,
  }) => {
    test.skip(browserName === "webkit" && isMobile, "mobile WebKit has no input that stays unsettled between frames");
    test.setTimeout(120_000);
    const SEEK_FROM_PX = 1_500;
    const TRIGGERS_PX = [400, 200, 120, 80];
    /// Up past rounding and small, so the view stays near the band.
    const UP_PX = 4;
    const touch = browserName === "chromium";
    const cdp = touch ? await page.context().newCDPSession(page) : null;
    let attempts = 0;
    for (const trigger of TRIGGERS_PX) {
      await parkAboveTheEnd(page, SEEK_FROM_PX);
      const box = (await page.locator("#chat-scroll-container").boundingBox())!;
      const x = box.x + box.width / 2;
      let y = box.y + box.height / 2;
      const send = (type: string) =>
        cdp!.send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x, y }] });
      let fingerDown = false;
      if (!touch) await page.mouse.move(x, y);
      await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        const log: SeekEndEvent[] = [];
        const push = (kind: SeekEndEvent["kind"]) =>
          log.push({ kind, t: performance.now(), top: c.scrollTop, left: c.scrollHeight - c.clientHeight - c.scrollTop });
        // At the target, after the app's own listeners: the app has had each.
        c.addEventListener("scroll", () => push("scroll"));
        c.addEventListener("scrollend", () => push("end"));
        c.addEventListener("wheel", () => push("wheel"));
        c.addEventListener("touchstart", () => push("touch"));
        (window as unknown as { __seekEnd: SeekEndEvent[] }).__seekEnd = log;
        (document.querySelector('[data-testid="scroll-to-bottom"]') as HTMLElement).click();
      });
      try {
        let left = await distanceFromBottom(page);
        for (let i = 0; i < 400 && left > trigger && left > SCROLL_TOP_SLACK_PX; i++) left = await distanceFromBottom(page);
        let start = await scrollTop(page);
        const up = async () => start - (await scrollTop(page));
        if (touch) {
          await send("touchStart");
          fingerDown = true;
          // The touch stops the animation; a frame already written still lands.
          await followRealFramesOwnClock(page, 2);
          start = await scrollTop(page);
          for (let i = 0; i < 60 && (await up()) <= UP_PX; i++) {
            y += 1;
            await send("touchMove");
            await page.waitForTimeout(16);
          }
        } else {
          for (let i = 0; i < 8 && (await up()) <= UP_PX; i++) {
            await page.mouse.wheel(0, -2);
            await page.waitForTimeout(16);
          }
        }
        await followRealFramesOwnClock(page, 10);
        const at = await newestVisibleRow(page);
        const before = await distanceFromBottom(page);
        await page.evaluate(() => {
          const c = document.getElementById("chat-scroll-container")!;
          (window as unknown as { __seekEnd: SeekEndEvent[] }).__seekEnd.push({
            kind: "deliver",
            t: performance.now(),
            top: c.scrollTop,
            left: c.scrollHeight - c.clientHeight - c.scrollTop,
          });
        });
        await followDeliverOwnClock(page);
        await afterLayoutSettles(page);
        const drift = at ? await rowDrift(page, at) : Infinity;
        const full = await page.evaluate(() => (window as unknown as { __seekEnd: SeekEndEvent[] }).__seekEnd);
        // Up to the arrival: its own snap is no part of the question.
        const log = full.slice(0, full.findIndex((e) => e.kind === "deliver"));

        // Frames move the view down, the reader's moves up: each scroll is
        // classed against the scroll before it.
        const scrollIdx = log.flatMap((e, i) => (e.kind === "scroll" ? [i] : []));
        const dir = new Map<number, "frame" | "reader" | "none">();
        scrollIdx.forEach((i, k) => {
          const d = k === 0 ? 0 : log[i].top - log[scrollIdx[k - 1]].top;
          dir.set(i, d > 0.5 ? "frame" : d < -0.5 ? "reader" : "none");
        });
        const takeover = scrollIdx.find((i) => dir.get(i) === "reader");
        const lastFrame = scrollIdx.filter((i) => dir.get(i) === "frame" && (takeover === undefined || i < takeover)).at(-1);
        const midFlight = takeover !== undefined && lastFrame !== undefined && log[lastFrame].left > SCROLL_TOP_SLACK_PX;
        const inputAt = log.findIndex((e) => e.kind === "touch" || e.kind === "wheel");
        // Every end after the input began: the scroll it follows, and whether
        // it came before or after the reader's first upward move.
        const endsAfterInput = log.flatMap((e, i) => {
          if (e.kind !== "end" || inputAt < 0 || i < inputAt) return [];
          const prev = scrollIdx.filter((j) => j < i).at(-1);
          const follows = prev === undefined ? "none" : dir.get(prev);
          return [`${follows}-end ${takeover !== undefined && i > takeover ? "after" : "before"} the takeover`];
        });
        const endAfter = takeover === undefined ? -1 : log.findIndex((e, i) => i > takeover && e.kind === "end");
        const endBetween =
          lastFrame !== undefined && takeover !== undefined && log.slice(lastFrame + 1, takeover).some((e) => e.kind === "end");
        const queued = midFlight && endAfter >= 0 && !endBetween;
        const inBand = before <= BOTTOM_THRESHOLD_PX - 40 - IN_PLACE_TOLERANCE_PX;
        const t0 = takeover !== undefined ? log[takeover].t : log[0]?.t ?? 0;
        const timeline = log.map((e) => `${e.kind}@${Math.round(e.t - t0)}(${Math.round(e.left)})`).join(" ");
        const description =
          `trigger ${trigger}px, ${touch ? "held finger" : "wheel"}: ${timeline}; ` +
          `takeover ${midFlight ? `mid-flight, the last frame ${Math.round(log[lastFrame!].left)}px from the end` : "not mid-flight"}; ` +
          `ends after the input: ${endsAfterInput.join(", ") || "none"}; ` +
          `frame's end after the takeover with none between: ${queued ? `OBSERVED at +${Math.round(log[endAfter].t - t0)}ms` : "not observed"}; ` +
          `${before.toFixed(1)}px above the end before the arrival${inBand ? " (in band)" : ""}, drift ${drift.toFixed(1)}px`;
        test.info().annotations.push({ type: "seek takeover", description });
        if (midFlight) attempts++;
        // A held finger has sent no end: any that reached the app is not the
        // reader's, and inside the band the arrival shows whether it settled.
        if (touch && midFlight && inBand) {
          expect(drift, `the arrival snapped a held finger after the takeover (${description})`).toBeLessThanOrEqual(
            IN_PLACE_TOLERANCE_PX,
          );
        }
      } finally {
        if (fingerDown) await send("touchEnd").catch(() => {});
      }
    }
    test.info().annotations.push({ type: "seek takeover", description: `${attempts} of ${TRIGGERS_PX.length} attempts mid-flight` });
  });
});

/// `followRealFrames` without the recorder: `n` native frames and the task after each.
function followRealFramesOwnClock(page: Page, n: number) {
  return page.evaluate(
    (n) =>
      new Promise<void>((resolve) => {
        let i = 0;
        const step = () =>
          requestAnimationFrame(() => setTimeout(() => (++i >= n ? resolve() : step()), 0));
        step();
      }),
    n,
  );
}

/// A short arrival (one join event) on the browser's own clock, once its row is in.
async function followDeliverOwnClock(page: Page) {
  const rows = page.locator("#chat-content [data-anchor-row][data-item-key]");
  const before = await rows.count();
  await callRiverTest(page, "appendJoinEvent");
  await expect(rows, "premise: the join event was delivered as a new row").toHaveCount(before + 1, { timeout: 5_000 });
}
