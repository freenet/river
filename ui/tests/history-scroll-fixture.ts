import { expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom } from "./example-room";

// Browser-test utilities for the history-scroll specs, in three sections:
//
// * seek clock: drives the scroll-to-latest animation on Playwright's clock,
//   one frame per step (conversation-seek-speed.spec.ts);
// * settle ordering: holds a gesture's `scrollend` until an arrival has landed
//   (conversation-autoscroll.spec.ts);
// * settle debounce: selects the 120ms no-`scrollend` fallback and observes its
//   timers (conversation-scroll-debounce.spec.ts).
//
// Nothing here writes application state or implements a follow policy. The
// in-page parts (anything passed to `page.evaluate` or `addInitScript`) must be
// self-contained, so each section keeps its own small DOM helpers. Their
// frame waits differ on purpose: the seek clock captures the native
// `requestAnimationFrame` before installing the clock, settle ordering runs on
// the real clock, and the debounce section, whose clock fakes
// `requestAnimationFrame`, waits on a ResizeObserver notification instead.

// ---- seek clock ----
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
            window.__riverTest!.appendMessage(guard.text);
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

// ---- settle ordering ----
//
// Calling `__riverTest.appendMessage` only REQUESTS a delivery (the hook defers
// it), so an arrival asked for from a scroll listener can land after the
// browser's own `scrollend` has already settled the gesture. Which comes first
// is the browser's business: on Linux CI, desktop WebKit settled first in every
// attempt of both wheel-gesture tests, and Firefox sometimes did too.
// A test that needs "the arrival lands while the gesture is still unsettled"
// therefore has to establish that order, not hope for it.
//
// `gateScrollendGesture` does, for one gesture on `#chat-scroll-container`:
//
// * it steps the view in-page, one `scrollTop` write per animation frame, so
//   protocol round trips cannot space the steps out past a settle interval;
// * a capture-phase `scrollend` listener on `window` (ahead of the app's own
//   listener on the container) consumes every native end on the container
//   while the gesture is held, so programmatic steps, which the browser settles
//   frame by frame, add up to ONE gesture;
// * it requests the arrival from the first scroll event that reaches the
//   plan's delivery point, and stops stepping;
// * once the arrival's row is in the DOM, and the frame that lays it out has
//   run (the app answers a patch from its ResizeObserver), it releases exactly
//   one marked `scrollend` to the app, and stops gating.
//
// Or, for a gesture cut short by hiding the chat (`hide`), it clicks the mobile
// panel opener at that point instead, keeps gating until the chat has no
// height, and releases the one `scrollend` on the frame after: the settle comes
// while hidden, never while the chat is shown.
//
// Only one runs at a time: a gesture refuses to start while another one's gate
// is still in place, so nothing of an earlier one (in an earlier room, say) can
// reach the next.
//
// This proves what the app does for that order. It does not prove a native
// gesture always produces it; the native-input smoke tests cover real wheel and
// touch without requiring an order.

/// One controlled gesture: where to step, what to do, and when.
export type GateScrollendPlan = {
  /// `scrollTop` for each frame, as offsets from where the gesture starts.
  path: number[];
  /// Act at the first scroll event that has moved up `upPx` from the start
  /// (`"up"`), or that has done so and come back to the end (`"back-at-end"`).
  deliverWhen: "up" | "back-at-end";
  upPx: number;
} & (
  | {
      /// The inbound message to deliver. Its first 40 characters must be unique.
      text: string;
    }
  | {
      /// Or hide the chat with this mobile panel opener, and settle while hidden.
      hide: "hamburger-rooms-button" | "header-members-button";
    }
);

/// What the controlled gesture saw.
export type GateScrollendRun = {
  /// In order: `input` (a step written), `scroll` (an event on the container),
  /// `deliver` (the hook was asked for the arrival), `patch` (its row is in the
  /// DOM), or for `hide`: `hide` (the opener clicked) and `hidden` (the chat has
  /// no height); `held` (a native `scrollend` consumed before it reached the
  /// app), `settle` (the released `scrollend`, after the app handled it) and
  /// `settle (native)` (a native one that reached the app: after the release,
  /// unless the gate failed). A settle that reached the app while the chat had
  /// no height is tagged ` (hidden)`.
  events: string[];
  /// How far each scroll event moved the view, up to the delivery.
  steps: number[];
  /// The newest message row with any part in view at the delivery, and how far
  /// its top sat above the container's bottom edge.
  at: { id: string; gap: number } | null;
  /// The plan's action ran: the arrival was requested, or the opener clicked.
  delivered: boolean;
  released: boolean;
  /// The same row's position, and scrollHeight - scrollTop - clientHeight, as
  /// the settle was released: what the arrival did while the gesture was held.
  /// Null for `hide`, where the hidden chat has no geometry.
  atRelease: { id: string; gap: number } | null;
  distanceAtRelease: number | null;
};

declare global {
  interface Window {
    /// `gateScrollendGesture`'s teardown, present while a gesture runs.
    __riverSettleGate?: { stop(): void };
  }
}

/// Runs in the page, so it is self-contained.
function gateScrollendInPage(plan: GateScrollendPlan): Promise<GateScrollendRun> {
  return new Promise<GateScrollendRun>((resolve, reject) => {
    if (window.__riverSettleGate) return reject(new Error("a controlled gesture's gate is still in place"));
    const c = document.getElementById("chat-scroll-container")!;
    const history = document.querySelector('[data-testid="conversation-history"]')!;
    const run: GateScrollendRun = {
      events: [],
      steps: [],
      at: null,
      delivered: false,
      released: false,
      atRelease: null,
      distanceAtRelease: null,
    };
    const note = (what: string) => run.events.push(what);
    const releasedEnds = new WeakSet<Event>();
    const start = c.scrollTop;
    let last = start;
    let wentUp = false;
    let gating = true;
    let frame = 0;
    let raf = 0;
    let done = false;
    const timers: number[] = [];

    const gapOf = (id: string) => {
      const row = document.getElementById(id);
      if (!row || !c.contains(row)) return null;
      return { id, gap: c.getBoundingClientRect().bottom - row.getBoundingClientRect().top };
    };
    const newestVisible = () => {
      const box = c.getBoundingClientRect();
      let found: { id: string; gap: number } | null = null;
      for (const row of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
        const r = row.getBoundingClientRect();
        if (r.bottom > box.top && r.top < box.bottom) found = { id: row.id, gap: box.bottom - r.top };
      }
      return found;
    };
    const marker = "text" in plan ? plan.text.slice(0, 40) : null;
    const landed = () =>
      marker !== null &&
      Array.from(history.querySelectorAll<HTMLElement>('[id^="msg-"]')).some((row) =>
        row.textContent?.includes(marker),
      );

    // Ahead of the app: capture on `window` runs before any listener at the target.
    const gate = (e: Event) => {
      if (e.target !== c || releasedEnds.has(e) || !gating) return;
      e.stopImmediatePropagation();
      note("held");
    };
    // At the target, registered after the app's listener: runs once the app has.
    const received = (e: Event) =>
      note(`${releasedEnds.has(e) ? "settle" : "settle (native)"}${c.clientHeight === 0 ? " (hidden)" : ""}`);
    const onScroll = () => {
      note("scroll");
      if (run.delivered) return;
      const top = c.scrollTop;
      run.steps.push(top - last);
      last = top;
      if (start - top >= plan.upPx) wentUp = true;
      const ready =
        plan.deliverWhen === "up" ? wentUp : wentUp && c.scrollHeight - c.clientHeight - top <= 1;
      if (!ready) return;
      run.delivered = true;
      run.at = newestVisible();
      if ("text" in plan) {
        note("deliver");
        window.__riverTest!.appendMessage(plan.text);
        return;
      }
      const opener = Array.from(document.querySelectorAll<HTMLElement>(`[data-testid="${plan.hide}"]`)).find(
        (el) => el.getClientRects().length > 0,
      );
      if (!opener) return finish();
      note("hide");
      opener.click();
      raf = requestAnimationFrame(untilHidden);
    };
    // The app hides the chat on its next render, not in the click.
    const untilHidden = () => {
      if (c.clientHeight > 0) {
        raf = requestAnimationFrame(untilHidden);
        return;
      }
      note("hidden");
      raf = requestAnimationFrame(release);
    };
    const release = () => {
      gating = false;
      run.released = true;
      if ("text" in plan) {
        run.atRelease = run.at && gapOf(run.at.id);
        run.distanceAtRelease = c.scrollHeight - c.scrollTop - c.clientHeight;
      }
      const end = new Event("scrollend");
      releasedEnds.add(end);
      c.dispatchEvent(end);
      // Long enough for a late native end to show up in the record.
      timers.push(window.setTimeout(finish, 300));
    };
    const patched = new MutationObserver(() => {
      if (!run.delivered || !landed()) return;
      patched.disconnect();
      note("patch");
      // The frame that lays the patch out runs the app's restore (ResizeObserver
      // delivery follows animation frames), so release on the frame after it.
      raf = requestAnimationFrame(() => {
        raf = requestAnimationFrame(release);
      });
    });
    const step = () => {
      if (run.delivered) return;
      // The previous step's scroll event was dispatched before this frame's
      // callbacks: out of path and still not delivered means it never will be.
      if (frame >= plan.path.length) return finish();
      c.scrollTop = start + plan.path[frame++];
      note("input");
      raf = requestAnimationFrame(step);
    };
    function finish() {
      if (done) return;
      done = true;
      gating = false;
      cancelAnimationFrame(raf);
      timers.forEach((t) => clearTimeout(t));
      patched.disconnect();
      window.removeEventListener("scrollend", gate, { capture: true });
      c.removeEventListener("scrollend", received);
      c.removeEventListener("scroll", onScroll);
      delete window.__riverSettleGate;
      resolve(run);
    }

    window.__riverSettleGate = { stop: finish };
    window.addEventListener("scrollend", gate, { capture: true });
    c.addEventListener("scrollend", received);
    c.addEventListener("scroll", onScroll);
    patched.observe(history, { childList: true, subtree: true, characterData: true });
    timers.push(window.setTimeout(finish, 5_000));
    raf = requestAnimationFrame(step);
  });
}

/// Run one controlled gesture (see the section comment). Every listener,
/// observer and timer it installs is removed before it returns, whatever happens.
export async function gateScrollendGesture(page: Page, plan: GateScrollendPlan): Promise<GateScrollendRun> {
  try {
    return await page.evaluate(gateScrollendInPage, plan);
  } finally {
    await page.evaluate(() => window.__riverSettleGate?.stop()).catch(() => {});
  }
}

/// The record as one line, in order: `input → scroll → held → ... → settle`.
export function gateScrollendTimeline(run: GateScrollendRun): string {
  const at = run.distanceAtRelease === null ? "" : ` (released ${run.distanceAtRelease.toFixed(1)}px above the end)`;
  return `${run.events.join(" → ")}${at}`;
}

/// The order the controlled gesture exists to establish: the reader's input
/// scrolled the view, the arrival was requested and its row landed, and only
/// then did the app receive a settle, the released one, ahead of any native end.
/// For `hide`: the opener was clicked, the chat lost its height, and the first
/// settle the app received was the released one, while hidden; none reached it
/// while the chat was shown.
export function gateScrollendExpectOrder(run: GateScrollendRun) {
  const timeline = gateScrollendTimeline(run);
  const hide = run.events.includes("hide");
  expect(run.delivered, `premise: the gesture reached its delivery point (${timeline})`).toBe(true);
  expect(
    run.released,
    `premise: ${hide ? "the chat was hidden" : "the arrival landed"} and the settle was released (${timeline})`,
  ).toBe(true);
  const firsts: string[] = [];
  for (const e of run.events) {
    if (e !== "held" && !firsts.includes(e)) firsts.push(e);
    if (e.startsWith("settle")) break;
  }
  if (!hide) {
    expect(
      firsts,
      `premise: input → scroll → deliver → patch → settle, with no settle reaching the app before the patch (${timeline})`,
    ).toEqual(["input", "scroll", "deliver", "patch", "settle"]);
    return;
  }
  expect(
    firsts,
    `premise: input → scroll → hide → hidden → settle (hidden), with no settle reaching the app before the hide (${timeline})`,
  ).toEqual(["input", "scroll", "hide", "hidden", "settle (hidden)"]);
  expect(
    run.events.filter((e) => e.startsWith("settle") && !e.endsWith("(hidden)")),
    `premise: no settle reached the app while the chat was shown (${timeline})`,
  ).toEqual([]);
}

// ---- settle debounce ----
//
// Where a browser has no `scrollend` (Safari before 17.4), the history settles a
// gesture at the reader's quiet deadline instead: 120ms after their last move
// (`SCROLL_SETTLE_DEBOUNCE_MS` in ui/src/components/conversation/history_scroll.rs),
// re-armed by every scroll the app reads as the reader's and by nothing of its
// own. Every engine in the suite has `scrollend`, so the fallback only runs
// when a test selects it:
//
// * `install` asks `Reflect.has(container, "onscrollend")`. The init script
//   below answers false for exactly that question, about exactly
//   `#chat-scroll-container`, and delegates every other lookup. No native
//   property is removed: the engine still sends `scrollend`, the app just has no
//   listener for it.
// * The app's fallback registrations are recognised by where they come from: a
//   `setTimeout(settle, 120)` made while a `scroll` event on that container is
//   being dispatched (the reader's move, read in the app's listener), with the
//   same callback every time (the one quiet-deadline closure). Their clears and
//   firings are recorded; every other timer passes straight through.
// * Playwright's clock runs every timer, so the test decides when 120ms have
//   passed. It also runs `crate::util::defer`'s `setTimeout(0)`: an inbound
//   delivery and any deferred signal write wait for an explicit advance.
//   Native events (scroll, ResizeObserver) still come from real frames, so
//   `debounceFrames` waits on those, never on a timer or `requestAnimationFrame`
//   (both are faked).

/// The settle debounce's delay, as SCROLL_SETTLE_DEBOUNCE_MS.
export const DEBOUNCE_SETTLE_MS = 120;

/// One fallback registration the app made: when (on the page's clock), and what
/// became of it. `seq` orders registrations, clears and firings against each
/// other.
export type DebounceRegistration = {
  handle: number;
  at: number;
  seq: number;
  cleared: { at: number; seq: number; duringScroll: boolean } | null;
  fired: { at: number; seq: number } | null;
};

export type DebounceState = {
  /// The page's clock now.
  now: number;
  /// How often the app asked whether the container has `onscrollend` (each
  /// answered false).
  lookups: number;
  /// Event types the app added to the container after that lookup, during the
  /// rest of `install`.
  installListeners: string[];
  registrations: DebounceRegistration[];
  /// 120ms timers from the container's scroll dispatch with another callback:
  /// should stay 0, or the recogniser above is matching something else.
  strayCallbacks: number;
  /// Reflect.has on a non-container element, overridden vs native, to show the
  /// override is scoped.
  otherLookupDelegated: boolean;
};

type DebounceRow = { id: string; gap: number };

type DebounceDom = {
  probe: Omit<DebounceState, "now" | "otherLookupDelegated">;
  restore(): void;
  /// Resolve after `n` native rendering updates.
  frames(n: number): Promise<void>;
  newestVisible(): DebounceRow | null;
  /// Move the view to `top` as a reader would, and wait for its `scroll` event
  /// to have been handled (this listener runs after the app's).
  scrollTo(top: number): Promise<{ landed: number; at: DebounceRow | null; scrolled: boolean }>;
};

declare global {
  interface Window {
    __riverDebounce: DebounceDom;
  }
}

/// Runs in the page before the app (`addInitScript`), so it is self-contained.
function debounceInitScript() {
  const CONTAINER_ID = "chat-scroll-container";
  const SETTLE_MS = 120;
  const nativeHas = Reflect.has;
  const undo: (() => void)[] = [];
  const probe: DebounceDom["probe"] = {
    lookups: 0,
    installListeners: [],
    registrations: [],
    strayCallbacks: 0,
  };
  let seq = 0;
  let armed = false;
  const container = () => document.getElementById(CONTAINER_ID) as HTMLElement;
  const duringContainerScroll = (c: Element) => {
    const ev = (window as { event?: Event }).event;
    return !!ev && ev.type === "scroll" && ev.currentTarget === c;
  };

  // Wrapped at the lookup, not at load: by then Playwright's clock has replaced
  // the timer functions, so these wrap the clock's.
  const arm = (c: Element) => {
    if (armed) return;
    armed = true;
    // `install` adds the rest of its listeners synchronously after the lookup.
    let installing = true;
    queueMicrotask(() => (installing = false));
    const nativeAdd = c.addEventListener;
    c.addEventListener = function (this: Element, type: string, ...rest: unknown[]) {
      if (installing) probe.installListeners.push(String(type));
      return (nativeAdd as (...a: unknown[]) => void).call(this, type, ...rest);
    } as typeof c.addEventListener;
    undo.push(() => delete (c as { addEventListener?: unknown }).addEventListener);

    // Playwright's clock numbers its timers from 10^12. The app keeps a timeout
    // handle as an i32 (`settle_timer`), so that id arrives truncated and its
    // `clearTimeout` would miss, and every superseded settle would still fire.
    // A browser hands out small integers, so this does too, mapped to the clock's.
    const nativeSet = window.setTimeout as (...a: unknown[]) => number;
    const nativeClear = window.clearTimeout as (id?: number) => void;
    const clockIds = new Map<number, number>();
    let nextHandle = 1;
    const schedule = (cb: unknown, delay: unknown, args: unknown[], onFire?: () => void) => {
      const handle = nextHandle++;
      const run =
        typeof cb === "function"
          ? function (this: unknown, ...a: unknown[]) {
              clockIds.delete(handle);
              onFire?.();
              return (cb as (...a: unknown[]) => unknown).apply(this, a);
            }
          : cb;
      clockIds.set(handle, nativeSet.call(window, run, delay, ...args));
      return handle;
    };
    let settle: unknown = null;
    window.setTimeout = function (cb: unknown, delay?: number, ...args: unknown[]) {
      if (delay === SETTLE_MS && typeof cb === "function" && duringContainerScroll(c)) {
        settle ??= cb;
        if (cb === settle) {
          const rec: DebounceRegistration = { handle: 0, at: Date.now(), seq: seq++, cleared: null, fired: null };
          rec.handle = schedule(cb, delay, args, () => (rec.fired = { at: Date.now(), seq: seq++ }));
          probe.registrations.push(rec);
          return rec.handle;
        }
        probe.strayCallbacks++;
      }
      return schedule(cb, delay, args);
    } as typeof window.setTimeout;
    window.clearTimeout = function (handle?: number) {
      const rec = probe.registrations.find((r) => r.handle === handle && !r.cleared && !r.fired);
      if (rec) rec.cleared = { at: Date.now(), seq: seq++, duringScroll: duringContainerScroll(c) };
      const id = handle === undefined ? undefined : clockIds.get(handle);
      if (id === undefined) return nativeClear.call(window, handle);
      clockIds.delete(handle!);
      return nativeClear.call(window, id);
    } as typeof window.clearTimeout;
    undo.push(() => {
      window.setTimeout = nativeSet;
      window.clearTimeout = nativeClear;
    });
  };

  Reflect.has = function (target: object, key: PropertyKey) {
    if (key === "onscrollend" && target instanceof Element && target.id === CONTAINER_ID) {
      probe.lookups++;
      arm(target);
      return false;
    }
    return nativeHas(target, key);
  };
  undo.push(() => (Reflect.has = nativeHas));

  // One native rendering update: a ResizeObserver's first notification, which
  // comes after that update's scroll events. Chained through a MessageChannel
  // task, since observing from inside a notification would be skipped.
  const frame = () =>
    new Promise<void>((resolve) => {
      const ro = new ResizeObserver(() => {
        ro.disconnect();
        const ch = new MessageChannel();
        ch.port1.onmessage = () => {
          ch.port1.close();
          ch.port2.close();
          resolve();
        };
        ch.port2.postMessage(0);
      });
      ro.observe(document.body);
    });

  const dom: DebounceDom = {
    probe,
    restore() {
      while (undo.length) undo.pop()!();
    },
    async frames(n) {
      for (let i = 0; i < n; i++) await frame();
    },
    newestVisible() {
      const c = container();
      const box = c.getBoundingClientRect();
      let found: DebounceRow | null = null;
      for (const row of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
        const r = row.getBoundingClientRect();
        if (r.bottom > box.top && r.top < box.bottom) found = { id: row.id, gap: box.bottom - r.top };
      }
      return found;
    },
    async scrollTo(top) {
      const c = container();
      let scrolled = false;
      const seen = new Promise<void>((resolve) =>
        c.addEventListener("scroll", () => ((scrolled = true), resolve()), { once: true }),
      );
      c.scrollTop = top;
      const landed = c.scrollTop;
      const at = dom.newestVisible();
      await Promise.race([seen, dom.frames(10)]);
      await dom.frames(1);
      return { landed, at, scrolled };
    },
  };
  window.__riverDebounce = dom;
}

/// Select the fallback and take over the clock. Call before the first navigation.
export async function debounceUseFallback(page: Page) {
  await page.addInitScript(debounceInitScript);
  await page.clock.install();
}

/// Put everything back. The clock cannot be uninstalled; the page closes with
/// the test.
export async function debounceRestore(page: Page) {
  await page.evaluate(() => window.__riverDebounce?.restore()).catch(() => {});
}

export function debounceState(page: Page): Promise<DebounceState> {
  return page.evaluate(() => {
    const { probe } = window.__riverDebounce;
    const div = document.createElement("div");
    return {
      ...JSON.parse(JSON.stringify(probe)),
      now: Date.now(),
      otherLookupDelegated: Reflect.has(div, "onscrollend") === "onscrollend" in div,
    };
  });
}

/// Registrations neither cleared nor fired.
export function debouncePending(state: DebounceState): DebounceRegistration[] {
  return state.registrations.filter((r) => !r.cleared && !r.fired);
}

/// The one pending registration: the deadline that will settle the gesture.
export function debounceOnlyPending(state: DebounceState, why: string): DebounceRegistration {
  const pending = debouncePending(state);
  expect(pending.length, `${why}: exactly one settle pending (${JSON.stringify(state.registrations)})`).toBe(1);
  return pending[0];
}

/// Registrations that fired, from the `from`th on (`debouncePauseWhenQuiet`'s
/// result: the setup's own settles are not the test's).
export function debounceFired(state: DebounceState, from: number): DebounceRegistration[] {
  return state.registrations.slice(from).filter((r) => r.fired);
}

export function debounceFrames(page: Page, n = 2): Promise<void> {
  return page.evaluate((n) => window.__riverDebounce.frames(n), n);
}

/// Advance the page's clock by `ms`, running every timer due, then let the
/// native frames that work causes go by.
export async function debounceAdvance(page: Page, ms: number) {
  await page.clock.runFor(ms);
  await debounceFrames(page);
}

/// The fallback is what `install` chose: the app asked, was told no, and added
/// no `scrollend` listener. The setup's own scrolls are follow snaps, which are
/// not the reader's and arm nothing; each test checks its own reader scroll
/// armed the deadline (`expectFreshDeadline` and the like).
export async function debounceExpectFallbackSelected(page: Page) {
  const state = await debounceState(page);
  expect(state.lookups, "premise: the app asked whether the container has onscrollend").toBeGreaterThanOrEqual(1);
  expect(state.otherLookupDelegated, "premise: the override answers only for the container").toBe(true);
  expect(state.installListeners, "premise: the lookup was install's (its later listeners were seen)").toContain(
    "touchstart",
  );
  expect(state.installListeners, "premise: install added no scrollend listener").not.toContain("scrollend");
  expect(state.strayCallbacks, "premise: every 120ms timer from a scroll is the one settle").toBe(0);
}

/// With the clock still running, wait until no settle is pending, then pause it.
/// Returns how many registrations the setup made, for `debounceFired`.
export async function debouncePauseWhenQuiet(page: Page): Promise<number> {
  await expect
    .poll(async () => debouncePending(await debounceState(page)).length, {
      timeout: 5_000,
      message: "premise: the setup's settles should have run",
    })
    .toBe(0);
  const now = await page.evaluate(() => Date.now());
  await page.clock.pauseAt(now + 1_000);
  await debounceFrames(page, 3);
  // A restore landing after the poll (a late layout pass) scrolls and re-arms the
  // debounce on the paused clock: let it settle before the test starts.
  for (let i = 0; i < 5 && debouncePending(await debounceState(page)).length > 0; i++) {
    await debounceAdvance(page, DEBOUNCE_SETTLE_MS);
  }
  const state = await debounceState(page);
  expect(debouncePending(state), "premise: nothing pending once paused").toEqual([]);
  return state.registrations.length;
}

export function debounceDistanceFromBottom(page: Page): Promise<number> {
  return page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    return c.scrollHeight - c.scrollTop - c.clientHeight;
  });
}

export function debounceScrollTop(page: Page): Promise<number> {
  return page.evaluate(() => document.getElementById("chat-scroll-container")!.scrollTop);
}

export function debounceScrollTo(page: Page, top: number) {
  return page.evaluate((top) => window.__riverDebounce.scrollTo(top), top);
}

/// How far `before`'s row has moved from its gap; Infinity when it is gone or is
/// no longer the newest visible message.
export async function debounceDrift(page: Page, before: DebounceRow): Promise<number> {
  const now = await page.evaluate(() => window.__riverDebounce.newestVisible());
  return now?.id === before.id ? Math.abs(now.gap - before.gap) : Infinity;
}

/// Deliver an inbound message on a paused clock: request it, run the deferred
/// delivery (`setTimeout(0)`, due now, so no time passes), and wait for its row.
export async function debounceDeliver(page: Page, text: string) {
  await callRiverTest(page, "appendMessage", text);
  await page.clock.runFor(0);
  const row = page.getByText(text.slice(0, 40), { exact: false }).last();
  await expect(row, `premise: "${text.slice(0, 40)}" was delivered`).toBeAttached({ timeout: 5_000 });
  await debounceFrames(page);
}

/// The history's event-summary rows (join events and the like).
const DEBOUNCE_EVENT_ROWS = "#chat-content [data-anchor-row][data-item-key]";

/// Deliver a join event on a paused clock, as `debounceDeliver` does: a short
/// arrival (one event-summary row, 40px at 1280px wide, where a one-line
/// message with its header is ~100px and alone would leave the follow band).
/// Lands as a new row only when the history does not already end in one.
export async function debounceDeliverJoin(page: Page) {
  const rows = page.locator(DEBOUNCE_EVENT_ROWS);
  const before = await rows.count();
  await callRiverTest(page, "appendJoinEvent");
  await page.clock.runFor(0);
  await expect(rows, "premise: the join event was delivered as a new row").toHaveCount(before + 1, { timeout: 5_000 });
  await debounceFrames(page);
}

/// Open `roomName` at its newest message and add `fillers` messages to scroll
/// back through, on the running clock.
export async function debounceOpenFilledRoom(page: Page, roomName: string, path = "/", fillers = 8) {
  await page.goto(path);
  await waitForApp(page);
  await selectRoom(page, roomName);
  await expect(page.locator("#chat-scroll-container")).toBeVisible({ timeout: 5_000 });
  for (let i = 0; i < fillers; i++) {
    const text = `filler ${i}: ${"y".repeat(200)}`;
    await callRiverTest(page, "appendMessage", text);
    await expect(page.getByText(text.slice(0, 40), { exact: false }).last()).toBeVisible({ timeout: 5_000 });
  }
  await expect
    .poll(() => debounceDistanceFromBottom(page), {
      timeout: 5_000,
      message: "premise: the fillers should have been followed",
    })
    .toBeLessThanOrEqual(4);
}
