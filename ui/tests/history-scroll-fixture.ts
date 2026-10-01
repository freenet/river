import { Page } from "@playwright/test";

// Browser-test utilities for the history-scroll specs. Each section belongs to
// one scenario family; nothing here writes application state or implements a
// follow policy.

// ---- task 2: seek clock ----
//
// The scroll-to-latest animation is the app's own `requestAnimationFrame` loop
// (`on_seek_frame` in history_scroll.rs). Sampled at the host's cadence, how
// many frames land between a mid-flight arrival and the end depends on the
// machine: Linux CI WebKit produced 1/1/0 usable frames where a fast Mac
// produced a dozen. So the speed test drives the app's real frame callbacks
// with Playwright's clock, one 16ms frame per step, and lets the browser do a
// real rendering pass (scroll events, observers, the deferred inbound patch)
// between steps.
//
// Two records are kept apart on purpose: the frame where the test REQUESTED a
// delivery (`appendMessage` only schedules it, through `util::defer`), and the
// frame by which the message's row was OBSERVED in the DOM. Speeds after the
// arrival are measured from the observed patch, never from the request.

/// The clock's frame: Playwright's fake `requestAnimationFrame` fires on 16ms
/// boundaries, so a 16ms step runs exactly one animation frame.
export const SEEK_CLOCK_FRAME_MS = 16;

/// One sample, taken once a frame's work is over: the clock's time, `scrollTop`,
/// and the live end (`scrollHeight - clientHeight`).
export type SeekClockFrame = { t: number; top: number; max: number };

export type SeekClockRecord = {
  frames: SeekClockFrame[];
  /// Index of the frame after which the delivery was requested, or -1.
  requestedAt: number;
  /// Why the delivery guard refused to deliver, when it refused.
  declined: string | null;
  /// Index of the first frame sampled after the message's row was observed in
  /// the history, or -1 if it never was.
  patchAt: number;
  /// The live end at the moment the row was observed.
  patchMax: number;
  /// Whether the view came to rest after the patch (10 identical frames).
  stopped: boolean;
};

/// When the delivery guard fires: once the view has moved for `minMovedFrames`
/// consecutive frames and travelled `minTravelPx`, it delivers `text` if the
/// view is still more than `minDistancePx` above the end, and otherwise records
/// that it declined. Either way it decides once.
export type SeekClockGuard = {
  text: string;
  minMovedFrames: number;
  minTravelPx: number;
  minDistancePx: number;
};

type SeekClockRecorder = {
  record: SeekClockRecord;
  /// Sample the frame just finished, run the delivery guard, and report
  /// whether the run is over (stopped, or past `boundMs` of clock time).
  sample(): { done: boolean };
  /// Call `then` after the next real rendering pass and the task after it.
  afterRealFrame(then: () => void): void;
  stop(): void;
};

declare global {
  interface Window {
    __seekClockNative?: { raf: (cb: FrameRequestCallback) => number; isNative: boolean };
    __seekClockRecorder?: SeekClockRecorder;
  }
}

/// Install Playwright's clock for this page, keeping a native
/// `requestAnimationFrame` for `afterRealFrame`. Call before the first
/// navigation. Both are context init scripts, registered in this order, so the
/// capture runs before the clock replaces the function; `seekClockPause`
/// checks that it did.
export async function seekClockInstall(page: Page) {
  await page.context().addInitScript(() => {
    const raf = window.requestAnimationFrame;
    window.__seekClockNative = {
      raf: raf.bind(window),
      isNative: /\[native code\]/.test(Function.prototype.toString.call(raf)),
    };
  });
  await page.clock.install();
}

/// Stop the clock, a little ahead of now, once startup and setup are done. The
/// app's timers, deferred work and animation frames then run only inside
/// `seekClockRun`'s steps.
export async function seekClockPause(page: Page) {
  const { now, isNative } = await page.evaluate(() => ({
    now: Date.now(),
    isNative: window.__seekClockNative?.isNative ?? false,
  }));
  if (!isNative) throw new Error("seek clock: the native requestAnimationFrame was not captured before the clock");
  await page.clock.pauseAt(now + 500);
}

/// Runs in the page, so it must be self-contained.
function installSeekRecorder({ guard, boundMs }: { guard: SeekClockGuard; boundMs: number }) {
  const c = document.getElementById("chat-scroll-container")!;
  const history = document.querySelector('[data-testid="conversation-history"]')!;
  const record: SeekClockRecord = {
    frames: [],
    requestedAt: -1,
    declined: null,
    patchAt: -1,
    patchMax: Number.NaN,
    stopped: false,
  };
  const push = () => {
    record.frames.push({ t: performance.now(), top: c.scrollTop, max: c.scrollHeight - c.clientHeight });
  };
  const hasRow = () =>
    Array.from(history.querySelectorAll<HTMLElement>('[id^="msg-"]')).some((r) =>
      r.textContent?.includes(guard.text),
    );
  // Present before the delivery would make the patch meaningless.
  if (hasRow()) throw new Error("seek clock: the arrival's text is already in the history");
  const observer = new MutationObserver(() => {
    if (record.patchAt >= 0 || !hasRow()) return;
    record.patchAt = record.frames.length;
    record.patchMax = c.scrollHeight - c.clientHeight;
    observer.disconnect();
  });
  observer.observe(history, { childList: true, subtree: true, characterData: true });
  // Both ends of every channel still waiting on its message.
  const ports = new Set<MessagePort>();
  let decided = false;
  const moved = (k: number) => k >= 1 && record.frames[k].top > record.frames[k - 1].top;
  const recorder: SeekClockRecorder = {
    record,
    sample() {
      push();
      const { frames } = record;
      const n = frames.length;
      const { top, max, t } = frames[n - 1];
      if (!decided) {
        let run = 0;
        for (let k = n - 1; k >= 1 && moved(k); k--) run++;
        if (run >= guard.minMovedFrames && top - frames[0].top >= guard.minTravelPx) {
          decided = true;
          if (max - top > guard.minDistancePx) {
            (window as any).__riverTest.appendMessage(guard.text);
            record.requestedAt = n - 1;
          } else {
            record.declined = `frame ${n - 1} was only ${max - top}px above the end`;
          }
        }
      }
      const last = frames.slice(-10);
      record.stopped =
        record.patchAt >= 0 && last.length === 10 && last.every((f) => f.top === top && f.max === max);
      return { done: record.stopped || t - frames[0].t > boundMs };
    },
    afterRealFrame(then) {
      window.__seekClockNative!.raf(() => {
        const channel = new MessageChannel();
        ports.add(channel.port1).add(channel.port2);
        channel.port1.onmessage = () => {
          for (const p of [channel.port1, channel.port2]) {
            p.close();
            ports.delete(p);
          }
          then();
        };
        channel.port2.postMessage(null);
      });
    },
    stop() {
      observer.disconnect();
      for (const p of ports) p.close();
      ports.clear();
    },
  };
  push();
  window.__seekClockRecorder = recorder;
}

async function seekClockStop(page: Page): Promise<SeekClockRecord> {
  return page.evaluate(() => {
    const rec = window.__seekClockRecorder!;
    rec.stop();
    delete window.__seekClockRecorder;
    return rec.record;
  });
}

/// Press the scroll-to-latest button with the clock paused, then step the
/// animation one frame at a time until it rests after the arrival's patch, or
/// `boundMs` of clock time has passed. Frame 0 is the view at the press. The
/// clock is left paused; the caller resumes it.
export async function seekClockRun(
  page: Page,
  guard: SeekClockGuard,
  { boundMs = 2_000 }: { boundMs?: number } = {},
): Promise<SeekClockRecord> {
  await page.evaluate(installSeekRecorder, { guard, boundMs });
  let record: SeekClockRecord | undefined;
  try {
    await page.getByTestId("scroll-to-bottom").click();
    const maxSteps = Math.ceil(boundMs / SEEK_CLOCK_FRAME_MS) + 1;
    for (let i = 0; i < maxSteps; i++) {
      await page.clock.runFor(SEEK_CLOCK_FRAME_MS);
      // Sampled the moment the frame's timers have run; then a real rendering
      // pass and the task after it, so the frame's scroll event, observers and
      // any patch have been handled before the next frame.
      const done = await page.evaluate(
        () =>
          new Promise<boolean>((resolve) => {
            const rec = window.__seekClockRecorder!;
            const { done } = rec.sample();
            rec.afterRealFrame(() => resolve(done));
          }),
      );
      if (done) break;
    }
  } finally {
    record = await seekClockStop(page);
  }
  return record;
}

/// The same recorder on the browser's own clock: sampled after every native
/// frame, with the button pressed through the UI. For a smoke test that
/// scheduling on this engine delivers and finishes; it says nothing about speed.
export async function seekClockNativeRun(
  page: Page,
  guard: SeekClockGuard,
  { boundMs = 3_000 }: { boundMs?: number } = {},
): Promise<SeekClockRecord> {
  await page.evaluate(installSeekRecorder, { guard, boundMs });
  let record: SeekClockRecord | undefined;
  try {
    const done = page.evaluate(
      () =>
        new Promise<void>((resolve) => {
          window.__seekClockNative ??= { raf: window.requestAnimationFrame.bind(window), isNative: true };
          const rec = window.__seekClockRecorder!;
          const frame = () =>
            rec.afterRealFrame(() => {
              if (rec.sample().done) resolve();
              else frame();
            });
          frame();
        }),
    );
    // Settled below either way; keep a failed click from also leaving a stray rejection.
    done.catch(() => {});
    await page.getByTestId("scroll-to-bottom").click();
    await done;
  } finally {
    record = await seekClockStop(page);
  }
  return record;
}

/// Every frame as `ms-after-press:px-moved`, with `R` on the frame after which
/// the delivery was requested and `P` on the first frame sampled after its row
/// was observed.
export function seekClockLog(record: SeekClockRecord): string {
  const { frames, requestedAt, patchAt } = record;
  const t0 = frames[0]?.t ?? 0;
  const head =
    `requested after frame ${requestedAt}, patch observed before frame ${patchAt}` +
    (record.declined ? `, guard declined: ${record.declined}` : "") +
    `, ${record.stopped ? "stopped" : "NOT stopped"}`;
  const body = frames
    .map(
      (f, k) =>
        `${Math.round(f.t - t0)}ms:${k ? f.top - frames[k - 1].top : 0}` +
        `${k === requestedAt ? "R" : ""}${k === patchAt ? "P" : ""}(${f.max - f.top})`,
    )
    .join(" ");
  return `${head}; per frame ms:px(left): ${body}`;
}
