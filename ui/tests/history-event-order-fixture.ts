import { Page } from "@playwright/test";

// Browser-test utilities for conversation-anchor-events.spec.ts: put native
// events and layout changes in a chosen order relative to the app's own
// listeners, and keep one log of what reached the app. Everything runs on the
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
