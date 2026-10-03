import { Page } from "@playwright/test";

// Browser-test utilities for the conversation-* specs that need an exact event
// order: put native events and layout changes in a chosen order relative to the
// app's own listeners, and keep one log of what reached the app. Everything runs on the
// browser's own clock. Nothing here writes application state or implements a
// scroll policy; the in-page parts must be self-contained.
//
// The recorder's log (`orderLog`) holds every `scroll` and every `scrollend`
// (`end`) the container delivered, each recorded by a listener added after the
// app's, so an entry means the app has had the event, plus what the helpers
// below did.
//
// Every wait is on real rendering passes: `afterRealFrame` resolves after the
// next `requestAnimationFrame` and the task after it, so a step's `scroll`
// event and observers have reached the app before the next step.

/// One reader move: `scrollTop` before the write and as read back right after
/// it, and the live end then.
type ReaderMove = {
  before: number;
  after: number;
  max: number;
  /// Whether the move's `scroll` event reached the container (after the app's
  /// own listener) before the wait ended.
  delivered: boolean;
};

type OrderRecorder = {
  log: string[];
  /// After the next real rendering pass and the task after it.
  afterRealFrame(then: () => void): void;
  stop(): void;
};

declare global {
  interface Window {
    __historyOrder?: OrderRecorder;
  }
}

/// Start the log. Pair with `orderRecorderStop` in a `finally`.
export async function orderRecorderStart(page: Page) {
  await page.evaluate(() => {
    const ports = new Set<MessagePort>();
    const c = document.getElementById("chat-scroll-container")!;
    const log: string[] = [];
    // At the target, after the app's own listeners: the app has had the event.
    const onScroll = () => log.push("scroll");
    const onEnd = () => log.push("end");
    c.addEventListener("scroll", onScroll);
    c.addEventListener("scrollend", onEnd);
    window.__historyOrder = {
      log,
      afterRealFrame(then) {
        requestAnimationFrame(() => {
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
        for (const p of ports) p.close();
        ports.clear();
        c.removeEventListener("scroll", onScroll);
        c.removeEventListener("scrollend", onEnd);
      },
    };
  });
}

export async function orderRecorderStop(page: Page) {
  await page.evaluate(() => {
    window.__historyOrder?.stop();
    delete window.__historyOrder;
  });
}

/// What `recordMidflightArrival` saw, on one `performance.now()` clock.
export type MidflightArrival = {
  /// `scrollTop` and the live end at the click: the navigation's destination.
  start: number;
  destination: number;
  /// `scrollTop` when the arrivals were asked for, and when the last of them
  /// was in the DOM.
  atRequest: number;
  atAttachment: number;
  attached: string[];
  /// Every step in order: `click`, `request`, `attached`, each `scroll` and
  /// `end` the container delivered (after the app's own listeners), and
  /// `settled`, each `kind@scrollTop+ms` since the click.
  timeline: string;
  /// The rows were in the DOM before any `scrollend` and short of the
  /// destination, so the navigation had not finished.
  attachedBeforeEnd: boolean;
  /// A native `scroll` after the attachment moved the view further: the
  /// animation was still running with the rows below it.
  progressedAfterAttachment: boolean;
  /// Both: proven mid-flight attachment, by event order rather than distance.
  midflightAtAttachment: boolean;
};

/// How the probe adds the arrivals.
///
/// * `hook`: `window.__riverTest.appendMessage`, the real state change and
///   render. It is deferred, so on a fast animation the rows can attach after
///   the landing; the result then says so rather than claiming mid-flight.
/// * `synchronous`: clones of the newest history row, appended to
///   `#chat-content` after the history list (outside what Dioxus renders)
///   inside the `scroll` handler that requested them, so they are in
///   the DOM while that frame's animation is still running. Their ids and
///   anchor attributes are removed and they carry `data-synthetic-arrival`:
///   to the scroll model they are content growing below the view, which is
///   what an arrival is during a navigation. The margin matches the history's
///   own row spacing, so the click-time end is their top edge as it would be
///   for a real row. Remove them with `removeSyntheticArrivals`.
export type ArrivalAttachment = "hook" | "synchronous";

/// Click scroll-to-latest and, at the navigation's first native `scroll` that
/// has moved the view (`midflight`) or once it reached its destination
/// (`after-end`, the negative control), add `messages` below the view. Every
/// listener is installed before the click, so the start, the destination and
/// the whole timeline are the click's own. Resolves once the navigation has
/// settled after the attachment: its `scrollend`, or 400ms with no `scroll`.
export function recordMidflightArrival(
  page: Page,
  messages: string[],
  { mode = "midflight", attach = "hook" }: { mode?: "midflight" | "after-end"; attach?: ArrivalAttachment } = {},
) {
  return page.evaluate(
    ({ messages, mode, attach }) =>
      new Promise<MidflightArrival>((resolve, reject) => {
        const AT_END_PX = 4;
        const SETTLED_MS = 400;
        const c = document.getElementById("chat-scroll-container")!;
        const content = document.getElementById("chat-content")!;
        const start = c.scrollTop;
        const destination = c.scrollHeight - c.clientHeight;
        let t0 = performance.now();
        const timeline: string[] = [];
        const log = (kind: string) =>
          timeline.push(`${kind}@${c.scrollTop.toFixed(1)}+${(performance.now() - t0).toFixed(0)}ms`);
        const attached: string[] = [];
        let requested = false;
        let atRequest = Number.NaN;
        let atAttachment = Number.NaN;
        let endedBeforeAttachment = false;
        let progressed = false;
        let settleTimer = 0;
        const cleanup = () => {
          window.clearTimeout(timeout);
          window.clearTimeout(settleTimer);
          observer.disconnect();
          c.removeEventListener("scroll", onScroll);
          c.removeEventListener("scrollend", onEnd);
        };
        const timeout = window.setTimeout(() => {
          cleanup();
          reject(new Error(`arrival probe timed out: ${JSON.stringify({ start, destination, attached, timeline })}`));
        }, 10_000);
        let done = false;
        const settle = () => {
          if (done) return;
          done = true;
          log("settled");
          cleanup();
          const attachedBeforeEnd = !endedBeforeAttachment && atAttachment < destination - AT_END_PX;
          resolve({
            start,
            destination,
            atRequest,
            atAttachment,
            attached,
            timeline: timeline.join(" "),
            attachedBeforeEnd,
            progressedAfterAttachment: progressed,
            midflightAtAttachment: attachedBeforeEnd && progressed,
          });
        };
        const armSettle = () => {
          window.clearTimeout(settleTimer);
          settleTimer = window.setTimeout(settle, SETTLED_MS);
        };
        const noteAttached = () => {
          if (attached.length !== messages.length || !Number.isNaN(atAttachment)) return;
          atAttachment = c.scrollTop;
          log("attached");
          armSettle();
        };
        const observer = new MutationObserver(() => {
          for (const text of messages) {
            if (!attached.includes(text) && content.textContent?.includes(text)) attached.push(text);
          }
          noteAttached();
        });
        observer.observe(content, { childList: true, subtree: true, characterData: true });
        const request = () => {
          requested = true;
          atRequest = c.scrollTop;
          log("request");
          if (attach === "hook") {
            const hooks = (window as Window & { __riverTest?: { appendMessage(text: string): void } }).__riverTest;
            if (!hooks) throw new Error("arrival probe: window.__riverTest is unavailable");
            for (const text of messages) hooks.appendMessage(text);
            return;
          }
          const rows = content.querySelectorAll<HTMLElement>('[data-testid="conversation-history"] > *');
          const template = rows[rows.length - 1];
          if (!template) throw new Error("arrival probe: no history row to clone");
          for (const text of messages) {
            const row = template.cloneNode(true) as HTMLElement;
            for (const el of [row, ...row.querySelectorAll<HTMLElement>("*")]) {
              el.removeAttribute("id");
              el.removeAttribute("data-anchor-row");
              el.removeAttribute("data-testid");
              el.classList.remove("anchor-row");
            }
            row.setAttribute("data-synthetic-arrival", text);
            // `space-y-4` between history rows: the gap above a new last row.
            row.style.margin = "1rem 0 0";
            content.appendChild(row);
          }
          // The MutationObserver reports these after this handler returns; the
          // rows are in the DOM now, so attachment is now.
          for (const text of messages) attached.push(text);
          noteAttached();
        };
        const onScroll = () => {
          log("scroll");
          const top = c.scrollTop;
          if (!Number.isNaN(atAttachment)) {
            if (top > atAttachment + 1) progressed = true;
            armSettle();
          } else if (!requested) {
            const due =
              mode === "midflight" ? top > start + 1 && top < destination - AT_END_PX : top >= destination - AT_END_PX;
            if (due) request();
          }
        };
        const onEnd = () => {
          log("end");
          if (Number.isNaN(atAttachment)) endedBeforeAttachment = true;
          else return void window.setTimeout(settle, 0);
          if (mode === "after-end" && !requested) request();
        };
        // Added after the app's own listeners: the app has had each event.
        c.addEventListener("scroll", onScroll);
        c.addEventListener("scrollend", onEnd);
        const button = document.querySelector<HTMLElement>('[data-testid="scroll-to-bottom"]');
        if (!button) {
          cleanup();
          reject(new Error("arrival probe: scroll-to-bottom button is unavailable"));
          return;
        }
        t0 = performance.now();
        log("click");
        button.click();
      }),
    { messages, mode, attach },
  );
}

/// Remove what an `attach: "synchronous"` probe inserted. Safe in `finally`.
export function removeSyntheticArrivals(page: Page) {
  return page.evaluate(() => {
    for (const row of document.querySelectorAll("[data-synthetic-arrival]")) row.remove();
  });
}

/// The event log so far, as one line.
export function orderLog(page: Page): Promise<string> {
  return page.evaluate(() => window.__historyOrder!.log.join(" "));
}

/// Let `n` real rendering passes go by (and the task after each).
export function orderRealFrames(page: Page, n: number) {
  return page.evaluate(
    (n) =>
      new Promise<void>((resolve) => {
        const rec = window.__historyOrder!;
        let i = 0;
        const wait = () => rec.afterRealFrame(() => (++i >= n ? resolve() : wait()));
        wait();
      }),
    n,
  );
}

/// Move the view by `px` (negative is up) with one `scrollTop` write, and wait
/// until its `scroll` event has reached the app, or five real frames.
export function orderReaderMove(page: Page, px: number) {
  return page.evaluate(
    (px) =>
      new Promise<ReaderMove>((resolve) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__historyOrder!;
        const before = c.scrollTop;
        let delivered = false;
        // Added after the app's listener, so it runs once the app has the event.
        const onScroll = () => (delivered = true);
        c.addEventListener("scroll", onScroll);
        c.scrollTop = before + px;
        const move: ReaderMove = { before, after: c.scrollTop, max: c.scrollHeight - c.clientHeight, delivered: false };
        let frames = 0;
        const wait = () =>
          rec.afterRealFrame(() => {
            if (!delivered && ++frames < 5) return wait();
            c.removeEventListener("scroll", onScroll);
            move.delivered = delivered;
            resolve(move);
          });
        wait();
      }),
    px,
  );
}

/// In one task: grow the newest message row wholly above the view by `px`
/// (`padding-top`), then dispatch a synthetic `scrollend` on the container, so
/// the end reaches the app before any ResizeObserver can report the growth.
/// Logs `grow`, `end` (the recorder's, after the app's listener) and `observed`
/// (this helper's own observer on `#chat-content`, its first delivery). Resolves
/// once it has delivered, with `scrollTop` before the growth, after it, and
/// after the app handled the end, and how far row `id` moved in the growth
/// (positive is down, before the app saw anything).
export function orderGrowAboveThenEnd(page: Page, px: number, id: string) {
  return page.evaluate(
    ({ px, id }) =>
      new Promise<{ before: number; grown: number; ended: number; shift: number }>((resolve) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rec = window.__historyOrder!;
        const top = c.getBoundingClientRect().top;
        const above = Array.from(c.querySelectorAll<HTMLElement>('[id^="msg-"]'))
          .filter((r) => r.getBoundingClientRect().bottom < top)
          .at(-1);
        if (!above) throw new Error("event order: no message row is entirely above the view");
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

/// What a scroll-to-latest navigation's end met, applied in the same task just
/// before the app handles that end: `grow` pads the newest message row wholly
/// above the view by `px`, `hide` takes the reading anchor out of the rendered
/// rows.
export type EndReflowKind = "grow" | "hide";

/// What the fixture's `run` observed: the newest visible message before the
/// change, the view and live end, and how far `grow` moved that message down.
export type EndReflow = {
  kind: EndReflowKind;
  top: number;
  max: number;
  anchor: { id: string; gap: number } | null;
  shift: number;
};

type EndReflowFixture = {
  result: EndReflow | null;
  /// Apply `kind` now. Logs it to the order recorder, when one runs, and logs
  /// `observed` at the first ResizeObserver delivery after it.
  run(kind: EndReflowKind, px: number): EndReflow;
  /// Put back what `hide` took away.
  unhide(): void;
};

declare global {
  interface Window {
    __historyEndReflow?: EndReflowFixture;
  }
}

/// Install `window.__historyEndReflow`. Runs in the page (`addInitScript`), so
/// it is self-contained.
///
/// `hide` renames the newest visible anchor row's `data-anchor-row` rather than
/// removing it: the row and its layout stay, so the rest of the history is
/// untouched, but the saved key is no longer rendered, which is all "missing"
/// means to the scroll model. A real deletion cannot be timed into one task:
/// the `removeMessages` hook defers its state change and the render follows it.
function installEndReflow() {
  const intersecting = (box: DOMRect, row: DOMRect) => row.bottom > box.top && row.top < box.bottom;
  let hidden: { row: HTMLElement; key: string } | null = null;
  const fixture: EndReflowFixture = {
    result: null,
    run(kind, px) {
      const c = document.getElementById("chat-scroll-container")!;
      const box = c.getBoundingClientRect();
      const messages = Array.from(c.querySelectorAll<HTMLElement>('[id^="msg-"]'));
      const visible = messages.filter((r) => intersecting(box, r.getBoundingClientRect())).at(-1);
      const result: EndReflow = {
        kind,
        top: c.scrollTop,
        max: c.scrollHeight - c.clientHeight,
        anchor: visible ? { id: visible.id, gap: box.bottom - visible.getBoundingClientRect().top } : null,
        shift: 0,
      };
      if (kind === "grow") {
        const above = messages.filter((r) => r.getBoundingClientRect().bottom < box.top).at(-1);
        if (!above || !visible) throw new Error("end reflow: no message row wholly above a visible one");
        const before = visible.getBoundingClientRect().top;
        above.style.paddingTop = `${(parseFloat(above.style.paddingTop) || 0) + px}px`;
        result.shift = visible.getBoundingClientRect().top - before;
      } else {
        const rows = Array.from(c.querySelectorAll<HTMLElement>("#chat-content [data-anchor-row]"));
        const row = rows.filter((r) => intersecting(box, r.getBoundingClientRect())).at(-1);
        if (!row) throw new Error("end reflow: no anchor row is visible");
        const key = row.getAttribute("data-anchor-row")!;
        row.setAttribute("data-anchor-row", `${key}::hidden-by-test`);
        hidden = { row, key };
      }
      window.__historyOrder?.log.push(kind);
      const observer = new ResizeObserver(() => {
        observer.disconnect();
        window.__historyOrder?.log.push("observed");
      });
      observer.observe(document.getElementById("chat-content")!);
      fixture.result = result;
      return result;
    },
    unhide() {
      hidden?.row.setAttribute("data-anchor-row", hidden.key);
      hidden = null;
    },
  };
  window.__historyEndReflow = fixture;
}

/// Register `window.__historyEndReflow` for every later navigation.
export function registerEndReflow(page: Page): Promise<void> {
  return page.addInitScript(installEndReflow);
}

/// Apply `kind` in the capture phase of the history's next native `scrollend`,
/// before the app's own listener on the container runs.
export function endReflowAtNextScrollend(page: Page, kind: EndReflowKind, px = 0) {
  return page.evaluate(
    ({ kind, px }) => {
      const c = document.getElementById("chat-scroll-container")!;
      const onEnd = (e: Event) => {
        if (e.target !== c) return;
        window.removeEventListener("scrollend", onEnd, true);
        window.__historyEndReflow!.run(kind, px);
      };
      window.addEventListener("scrollend", onEnd, true);
    },
    { kind, px },
  );
}

/// What the end reflow did, once it has run.
export function endReflowResult(page: Page): Promise<EndReflow | null> {
  return page.evaluate(() => window.__historyEndReflow?.result ?? null);
}

/// Put back the anchor key `hide` renamed.
export function endReflowUnhide(page: Page) {
  return page.evaluate(() => window.__historyEndReflow?.unhide());
}
