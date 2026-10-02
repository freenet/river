import { expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { SEEK_CLOCK_FRAME_MS } from "./history-scroll-fixture";

// Browser-test utilities for conversation-follow-state.spec.ts: step the app's
// own frames on Playwright's clock (installed with `seekClockInstall`, paused
// with `seekClockPause`), move the view between them, and keep one timeline of
// both. Nothing here writes application state or implements a follow policy;
// the in-page parts must be self-contained.
//
// Every step waits for a real rendering pass and the task after it (on the
// native `requestAnimationFrame` the seek clock captured), so the step's
// `scroll` event and observers have reached the app before the next one.
//
// The recorder also keeps an event log (`followLog`): every `scroll` and every
// `scrollend` that reached the app (`end`), plus the test's own marks. With
// `gateEnds` it starts with a gate: a capture-phase `scrollend` listener on
// `window`, ahead of the app's, that consumes the container's native ends
// (`held`) until `followUngate` removes it (`followGate` puts it back). A gate is
// for setup only: an end the test is about must reach the app, so the test
// ungates first and checks the log for an `end` after its mark.
//
// With `observeSettle` (on the seek clock only) the recorder also watches the
// app's quiet-deadline callback. It is identified once, by its registration
// while a container `scrollend` is being dispatched to the app with a delay
// (a correction's end refused, the one native-mode path that arms it from an
// end); every registration of that same callback after it is recorded with its
// delay, clear and firing (`followSettleRecord`), wherever it is made.
// `followBeforeSettleFire` arms a one-shot action on it: just before its next
// firing, in the same task, it moves `scrollTop` and records what it saw. It
// only writes geometry; it dispatches nothing.

/// One line of the timeline: an app frame stepped on the clock, or a reader move.
/// `top` and `max` (the live end) are read once the step's work is over.
export type FollowEntry =
  | { kind: "frame"; top: number; max: number }
  | {
      kind: "input";
      /// `scrollTop` before the write, and as read back right after it.
      before: number;
      after: number;
      max: number;
      /// Whether the move's `scroll` event reached the container (after the
      /// app's own listener) before the step ended.
      delivered: boolean;
    };

/// One registration of the app's quiet-deadline callback (`observeSettle`), on
/// the page's clock.
export type FollowSettleTimer = {
  handle: number;
  at: number;
  delay: number;
  cleared: number | null;
  fired: number | null;
};

/// What a `followBeforeSettleFire` action saw, in the task the callback then ran in.
export type FollowBeforeFire = {
  handle: number;
  at: number;
  before: number;
  after: number;
  /// `scroll` events the app had had by then (its own move's cannot be among them).
  scrolls: number;
};

type FollowRecorder = {
  entries: FollowEntry[];
  log: string[];
  settle: FollowSettleTimer[];
  beforeFire: FollowBeforeFire[];
  /// Arm the one-shot action: move the view by `px` just before the callback fires.
  armBeforeFire(px: number): void;
  /// Undone by `stop`: listeners, observers and styles a helper put in place.
  cleanups: (() => void)[];
  /// Put the gate in place or remove it; whether it was in place before.
  setGate(on: boolean): boolean;
  /// After the next real rendering pass and the task after it.
  afterRealFrame(then: () => void): void;
  stop(): void;
};

declare global {
  interface Window {
    __followRecorder?: FollowRecorder;
  }
}

/// Start the timeline. Pair with `followRecorderStop` in a `finally`. Needs the
/// seek clock installed, unless `ownClock` says the test runs on the browser's.
export async function followRecorderStart(
  page: Page,
  { gateEnds = false, ownClock = false, observeSettle = false } = {},
) {
  await page.evaluate(({ gateEnds, ownClock, observeSettle }) => {
    if (ownClock && observeSettle) throw new Error("follow recorder: settle observation needs the seek clock");
    if (ownClock) window.__seekClockNative ??= { raf: window.requestAnimationFrame.bind(window), isNative: true };
    if (!window.__seekClockNative?.isNative) throw new Error("follow recorder: install the seek clock first");
    const raf = window.__seekClockNative.raf;
    const ports = new Set<MessagePort>();
    const c = document.getElementById("chat-scroll-container")!;
    const log: string[] = [];
    let gating = gateEnds;
    const gate = (e: Event) => {
      if (e.target !== c || !gating) return;
      e.stopImmediatePropagation();
      log.push("held");
    };
    // At the target, after the app's own listeners: the app has had the event.
    let scrolls = 0;
    const onScroll = () => {
      scrolls++;
      log.push("scroll");
    };
    const onEnd = () => log.push("end");
    const settle: FollowSettleTimer[] = [];
    const beforeFire: FollowBeforeFire[] = [];
    let settleCb: unknown = null;
    let lateMovePx: number | null = null;
    const duringAppEnd = () => {
      const ev = (window as { event?: Event }).event;
      return !!ev && ev.type === "scrollend" && ev.currentTarget === c;
    };
    const setGate = (on: boolean) => {
      const was = gating;
      gating = on;
      if (on) window.addEventListener("scrollend", gate, { capture: true });
      else window.removeEventListener("scrollend", gate, { capture: true });
      return was;
    };
    setGate(gateEnds);
    c.addEventListener("scroll", onScroll);
    c.addEventListener("scrollend", onEnd);
    // Playwright's clock numbers its timers from 10^12, and the app keeps a
    // timeout handle as an i32 (`settle_timer`), so its `clearTimeout` would
    // miss and a cancelled deadline would still fire. Hand out small handles,
    // as a browser does, mapped to the clock's.
    const clockSet = window.setTimeout as (...a: unknown[]) => number;
    const clockClear = window.clearTimeout as (id?: number) => void;
    const restoreTimers = () => {
      window.setTimeout = clockSet as typeof window.setTimeout;
      window.clearTimeout = clockClear as typeof window.clearTimeout;
    };
    if (!ownClock) {
      const ids = new Map<number, number>();
      let next = 1;
      window.setTimeout = function (cb: unknown, delay?: number, ...args: unknown[]) {
        const handle = next++;
        if (observeSettle && settleCb === null && typeof cb === "function" && (delay ?? 0) > 0 && duringAppEnd()) {
          settleCb = cb;
        }
        const timer: FollowSettleTimer | null =
          settleCb !== null && cb === settleCb
            ? { handle, at: Date.now(), delay: Number(delay ?? 0), cleared: null, fired: null }
            : null;
        if (timer) settle.push(timer);
        const run =
          typeof cb === "function"
            ? function (this: unknown, ...a: unknown[]) {
                ids.delete(handle);
                if (timer) {
                  if (lateMovePx !== null) {
                    const px = lateMovePx;
                    lateMovePx = null;
                    const before = c.scrollTop;
                    c.scrollTop = before + px;
                    beforeFire.push({ handle, at: Date.now(), before, after: c.scrollTop, scrolls });
                    log.push("late move");
                  }
                  timer.fired = Date.now();
                  log.push("deadline");
                }
                return (cb as (...a: unknown[]) => unknown).apply(this, a);
              }
            : cb;
        ids.set(handle, clockSet.call(window, run, delay, ...args));
        return handle;
      } as typeof window.setTimeout;
      window.clearTimeout = function (handle?: number) {
        const timer = settle.find((t) => t.handle === handle && t.cleared === null && t.fired === null);
        if (timer) timer.cleared = Date.now();
        const id = handle === undefined ? undefined : ids.get(handle);
        if (id === undefined) return clockClear.call(window, handle);
        ids.delete(handle!);
        return clockClear.call(window, id);
      } as typeof window.clearTimeout;
    }
    window.__followRecorder = {
      entries: [],
      log,
      settle,
      beforeFire,
      cleanups: [],
      armBeforeFire(px) {
        if (!observeSettle) throw new Error("follow recorder: started without observeSettle");
        lateMovePx = px;
      },
      setGate,
      afterRealFrame(then) {
        raf(() => {
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
        lateMovePx = null;
        for (const undo of this.cleanups.splice(0).reverse()) undo();
        for (const p of ports) p.close();
        ports.clear();
        setGate(false);
        if (!ownClock) restoreTimers();
        c.removeEventListener("scroll", onScroll);
        c.removeEventListener("scrollend", onEnd);
      },
    };
  }, { gateEnds, ownClock, observeSettle });
}

/// The quiet-deadline registrations and before-fire records so far (`observeSettle`).
export function followSettleRecord(page: Page): Promise<{ now: number; settle: FollowSettleTimer[]; beforeFire: FollowBeforeFire[] }> {
  return page.evaluate(() => {
    const rec = window.__followRecorder!;
    return JSON.parse(JSON.stringify({ now: Date.now(), settle: rec.settle, beforeFire: rec.beforeFire }));
  });
}

/// Arm the one-shot action (`observeSettle`): just before the quiet-deadline
/// callback next fires, move the view by `px` (negative is up), with no event of
/// its own, and log `late move` then `deadline`.
export function followBeforeSettleFire(page: Page, px: number) {
  return page.evaluate((px) => window.__followRecorder!.armBeforeFire(px), px);
}

export async function followRecorderStop(page: Page): Promise<FollowEntry[]> {
  return page.evaluate(() => {
    const rec = window.__followRecorder;
    if (!rec) return [];
    rec.stop();
    delete window.__followRecorder;
    return rec.entries;
  });
}

/// The event log so far, as one line.
export function followLog(page: Page): Promise<string> {
  return page.evaluate(() => window.__followRecorder!.log.join(" "));
}

/// Add `label` to the event log.
export function followMark(page: Page, label: string) {
  return page.evaluate((label) => void window.__followRecorder!.log.push(label), label);
}

/// Remove the gate, and mark the log `ungated`. Returns whether it was in place.
export function followUngate(page: Page): Promise<boolean> {
  return page.evaluate(() => {
    const rec = window.__followRecorder!;
    const was = rec.setGate(false);
    rec.log.push("ungated");
    return was;
  });
}

/// Put the gate back, and mark the log `gated`. Returns whether it was in place.
export function followGate(page: Page): Promise<boolean> {
  return page.evaluate(() => {
    const rec = window.__followRecorder!;
    const was = rec.setGate(true);
    rec.log.push("gated");
    return was;
  });
}

/// Wait up to `frames` real frames for `what` to appear in the log after the
/// last `mark`. Whether it did.
export function followAwaitAfter(page: Page, mark: string, what: string, frames = 10): Promise<boolean> {
  return page.evaluate(
    ({ mark, what, frames }) =>
      new Promise<boolean>((resolve) => {
        const rec = window.__followRecorder!;
        const seen = () => rec.log.slice(rec.log.lastIndexOf(mark) + 1).includes(what);
        let n = 0;
        const wait = () =>
          rec.afterRealFrame(() => (seen() || ++n >= frames ? resolve(seen()) : wait()));
        wait();
      }),
    { mark, what, frames },
  );
}

/// Grow the newest message row that is entirely above the view by `px`
/// (`padding-top`), and mark the log `grow`. Returns `scrollTop` just before,
/// and as read in the same task (no clamp, or the browser moved the view).
export function followGrowAbove(page: Page, px: number) {
  return page.evaluate((px) => {
    const c = document.getElementById("chat-scroll-container")!;
    const top = c.getBoundingClientRect().top;
    const rows = Array.from(c.querySelectorAll<HTMLElement>('[id^="msg-"]'));
    const row = rows.filter((r) => r.getBoundingClientRect().bottom < top).at(-1);
    if (!row) throw new Error("follow: no message row is entirely above the view");
    const before = c.scrollTop;
    row.style.paddingTop = `${(parseFloat(row.style.paddingTop) || 0) + px}px`;
    window.__followRecorder!.log.push("grow");
    return { id: row.id, before, after: c.scrollTop };
  }, px);
}

/// In one task: grow the newest message row wholly above the view by `px`, then
/// dispatch a synthetic `scrollend` on the container, so the settle reaches the
/// app before any ResizeObserver can report the growth. Logs `grow`, `end` (the
/// recorder's, after the app's listener) and `observed` (this helper's own
/// observer on `#chat-content`, its first delivery). Resolves once it has
/// delivered, with `scrollTop` before the growth, after it, and after the
/// app handled the end, and how far row `id` moved in the growth (positive is
/// down, before the app saw anything).
export function followGrowAboveThenEnd(page: Page, px: number, id: string) {
  return page.evaluate(
    ({ px, id }) =>
      new Promise<{ before: number; grown: number; ended: number; shift: number }>((resolve) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__followRecorder!;
        const top = c.getBoundingClientRect().top;
        const above = Array.from(c.querySelectorAll<HTMLElement>('[id^="msg-"]'))
          .filter((r) => r.getBoundingClientRect().bottom < top)
          .at(-1);
        if (!above) throw new Error("follow: no message row is entirely above the view");
        const tracked = document.getElementById(id)!;
        const rowTop = () => tracked.getBoundingClientRect().top;
        const before = c.scrollTop;
        const rowBefore = rowTop();
        above.style.paddingTop = `${(parseFloat(above.style.paddingTop) || 0) + px}px`;
        rec.log.push("grow");
        const grown = c.scrollTop;
        const shift = rowTop() - rowBefore;
        c.dispatchEvent(new Event("scrollend"));
        const ended = c.scrollTop;
        // Observers deliver in the next rendering pass, the app's (made first)
        // before this one.
        const observer = new ResizeObserver(() => {
          observer.disconnect();
          rec.log.push("observed");
          rec.afterRealFrame(() => resolve({ before, grown, ended, shift }));
        });
        observer.observe(document.getElementById("chat-content")!);
      }),
    { px, id },
  );
}

/// What `followResizeBeforeEnd` saw: the container's `scrollTop` and height
/// just before and just after its resize, in the listener, and when it ran.
export type FollowResize = {
  ran: boolean;
  before: { top: number; height: number };
  after: { top: number; height: number };
};

/// Arm a one-shot, capture-phase `scrollend` listener on `window`, ahead of the
/// app's: at the container's next end it resizes the container by `px`
/// (positive shrinks, negative grows from an initially constrained height)
/// (`max-height`, removed by `followRecorderStop`), forces layout with a geometry
/// read and logs `resize`, so the app reads that end with the container already
/// resized and before any ResizeObserver can report it (the mutation and the
/// app's listener run in one task). It then logs `observed` at its own observer's
/// first delivery on the container, which comes after the app's (made first).
/// Read the record with `followResizeRecord`. The ordering is made, not native.
export function followResizeBeforeEnd(page: Page, px: number) {
  return page.evaluate((px) => {
    const c = document.getElementById("chat-scroll-container")!;
    const rec = window.__followRecorder!;
    const record: FollowResize = {
      ran: false,
      before: { top: Number.NaN, height: Number.NaN },
      after: { top: Number.NaN, height: Number.NaN },
    };
    (window as unknown as { __followResize: FollowResize }).__followResize = record;
    let observer: ResizeObserver | null = null;
    const onEnd = (e: Event) => {
      if (e.target !== c) return;
      window.removeEventListener("scrollend", onEnd, { capture: true });
      record.ran = true;
      record.before = { top: c.scrollTop, height: c.clientHeight };
      c.style.maxHeight = `${c.clientHeight - px}px`;
      record.after = { top: c.scrollTop, height: c.clientHeight };
      rec.log.push("resize");
      observer = new ResizeObserver(() => {
        observer!.disconnect();
        rec.log.push("observed");
      });
      observer.observe(c);
    };
    window.addEventListener("scrollend", onEnd, { capture: true });
    rec.cleanups.push(() => {
      window.removeEventListener("scrollend", onEnd, { capture: true });
      observer?.disconnect();
      c.style.removeProperty("max-height");
      delete (window as unknown as { __followResize?: FollowResize }).__followResize;
    });
  }, px);
}

export function followResizeRecord(page: Page): Promise<FollowResize> {
  return page.evaluate(() => (window as unknown as { __followResize: FollowResize }).__followResize);
}

/// The history's event-summary rows (join events and the like).
const EVENT_ROWS = "#chat-content [data-anchor-row][data-item-key]";

/// Deliver an inbound arrival on the paused clock: request it, run the deferred
/// delivery (`setTimeout(0)`, due now, so no time passes), wait for its row and
/// then two real frames. `"join"` is a short arrival: one event-summary row
/// (40px at 1280px wide), which lands as a new row only when the history does
/// not already end in one.
export async function followDeliver(page: Page, what: string | "join") {
  const rows = page.locator(EVENT_ROWS);
  const before = what === "join" ? await rows.count() : 0;
  if (what === "join") await callRiverTest(page, "appendJoinEvent");
  else await callRiverTest(page, "appendMessage", what);
  await page.clock.runFor(0);
  if (what === "join") {
    await expect(rows, "premise: the join event was delivered as a new row").toHaveCount(before + 1, {
      timeout: 5_000,
    });
  } else {
    await expect(page.getByText(what.slice(0, 40), { exact: false }).last(), "premise: delivered").toBeAttached({
      timeout: 5_000,
    });
  }
  await followMark(page, "patch");
  await followRealFrames(page, 2);
}

/// Let `n` real rendering passes go by (and the task after each).
export function followRealFrames(page: Page, n: number) {
  return page.evaluate(
    (n) =>
      new Promise<void>((resolve) => {
        const rec = window.__followRecorder!;
        let i = 0;
        const wait = () => rec.afterRealFrame(() => (++i >= n ? resolve() : wait()));
        wait();
      }),
    n,
  );
}

/// Run one app frame on the paused clock, then a real rendering pass.
export async function followFrame(page: Page) {
  await page.clock.runFor(SEEK_CLOCK_FRAME_MS);
  return page.evaluate(
    () =>
      new Promise<FollowEntry>((resolve) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__followRecorder!;
        const entry: FollowEntry = { kind: "frame", top: c.scrollTop, max: c.scrollHeight - c.clientHeight };
        rec.entries.push(entry);
        rec.afterRealFrame(() => resolve(entry));
      }),
  );
}

/// Move the view by `px` (negative is up) with one `scrollTop` write, and wait
/// until its `scroll` event has reached the app, or five real frames.
export function followReaderMove(page: Page, px: number) {
  return page.evaluate(
    (px) =>
      new Promise<FollowEntry>((resolve) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__followRecorder!;
        const before = c.scrollTop;
        let delivered = false;
        // Added after the app's listener, so it runs once the app has the event.
        const onScroll = () => (delivered = true);
        c.addEventListener("scroll", onScroll);
        c.scrollTop = before + px;
        const entry: FollowEntry = {
          kind: "input",
          before,
          after: c.scrollTop,
          max: c.scrollHeight - c.clientHeight,
          delivered: false,
        };
        let frames = 0;
        const wait = () =>
          rec.afterRealFrame(() => {
            if (!delivered && ++frames < 5) return wait();
            c.removeEventListener("scroll", onScroll);
            entry.delivered = delivered;
            rec.entries.push(entry);
            resolve(entry);
          });
        wait();
      }),
    px,
  );
}

/// The timeline as one line: `f+moved(left)` per frame, `i±moved(left)` per
/// input, with `!` on an input whose `scroll` event never arrived.
export function followTimeline(entries: FollowEntry[]): string {
  let prev: number | null = null;
  return entries
    .map((e) => {
      const top = e.kind === "frame" ? e.top : e.after;
      const moved = e.kind === "frame" ? (prev === null ? 0 : top - prev) : e.after - e.before;
      prev = top;
      const sign = moved >= 0 ? "+" : "";
      const tag = e.kind === "frame" ? "f" : "i";
      return `${tag}${sign}${+moved.toFixed(2)}(${Math.round(e.max - top)})${e.kind === "input" && !e.delivered ? "!" : ""}`;
    })
    .join(" ");
}

/// The newest message row with any part in view, and its top's gap above the
/// container's bottom edge.
export type RowPosition = { id: string; gap: number };

export function newestVisibleRow(page: Page): Promise<RowPosition | null> {
  return page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const box = c.getBoundingClientRect();
    let found: { id: string; gap: number } | null = null;
    for (const row of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
      const r = row.getBoundingClientRect();
      if (r.bottom > box.top && r.top < box.bottom) found = { id: row.id, gap: box.bottom - r.top };
    }
    return found;
  });
}

/// How far `before`'s row has moved from its gap; Infinity once another row is
/// the newest visible one, or it is gone.
export async function rowDrift(page: Page, before: RowPosition): Promise<number> {
  const now = await newestVisibleRow(page);
  return now?.id === before.id ? Math.abs(now.gap - before.gap) : Infinity;
}
