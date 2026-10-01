import { Page } from "@playwright/test";
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

type FollowRecorder = {
  entries: FollowEntry[];
  /// After the next real rendering pass and the task after it.
  afterRealFrame(then: () => void): void;
  stop(): void;
};

declare global {
  interface Window {
    __followRecorder?: FollowRecorder;
  }
}

/// Start the timeline. Pair with `followRecorderStop` in a `finally`.
export async function followRecorderStart(page: Page) {
  await page.evaluate(() => {
    if (!window.__seekClockNative?.isNative) throw new Error("follow recorder: install the seek clock first");
    const raf = window.__seekClockNative.raf;
    const ports = new Set<MessagePort>();
    window.__followRecorder = {
      entries: [],
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
        for (const p of ports) p.close();
        ports.clear();
      },
    };
  });
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
