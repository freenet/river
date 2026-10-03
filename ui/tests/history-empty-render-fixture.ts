import { expect, Page } from "@playwright/test";
import { afterLayoutSettles } from "./history-scroll-helpers";

type EmptyRender = {
  empty(keepHeight: boolean): { before: number; after: number };
  restore(): number | null;
};

declare global {
  interface Window {
    __riverEmptyRender?: EmptyRender;
  }
}

/// Simulate the history temporarily rendering no rows while retaining its DOM
/// nodes so the exact rows can be restored after the scroll model responds.
function installEmptyRender() {
  let detached: {
    fragment: DocumentFragment;
    placeholder: HTMLElement;
    observer: MutationObserver;
    mutations: number;
  } | null = null;

  window.__riverEmptyRender = {
    empty(keepHeight) {
      if (detached) throw new Error("empty render: already empty");
      const content = document.getElementById("chat-content")!;
      const sentinel = document.getElementById("bottom-sentinel");
      const before = content.getBoundingClientRect().height;
      const fragment = document.createDocumentFragment();
      for (const node of Array.from(content.childNodes)) {
        if (node !== sentinel) fragment.appendChild(node);
      }

      const placeholder = document.createElement("div");
      placeholder.style.height = keepHeight ? `${before}px` : "16rem";
      content.insertBefore(placeholder, sentinel?.parentNode === content ? sentinel : null);
      if (keepHeight) {
        const short = content.getBoundingClientRect().height - placeholder.getBoundingClientRect().height;
        placeholder.style.height = `${before - short}px`;
      }

      const observer = new MutationObserver((records) => {
        if (detached) detached.mutations += records.length;
      });
      observer.observe(fragment, { childList: true, subtree: true, attributes: true, characterData: true });
      detached = { fragment, placeholder, observer, mutations: 0 };
      return { before, after: content.getBoundingClientRect().height };
    },
    restore() {
      if (!detached) return null;
      const mutations = detached.mutations + detached.observer.takeRecords().length;
      detached.observer.disconnect();
      detached.placeholder.parentNode!.insertBefore(detached.fragment, detached.placeholder);
      detached.placeholder.remove();
      detached = null;
      return mutations;
    },
  };
}

/// Install the self-contained simulator before each page navigation.
export function registerEmptyRender(page: Page): Promise<void> {
  return page.addInitScript(installEmptyRender);
}

export function emptyHistory(page: Page, { keepHeight }: { keepHeight: boolean }) {
  return page.evaluate((height) => window.__riverEmptyRender!.empty(height), keepHeight);
}

/// Restore the exact detached nodes and report mutations observed while they
/// were detached. Safe in `finally`, including when no empty render began.
export function restoreHistory(page: Page): Promise<number | null> {
  return page.evaluate(() => window.__riverEmptyRender?.restore() ?? null);
}

export async function expectEmptyButVisible(page: Page) {
  expect(await page.evaluate(() => document.querySelectorAll("#chat-content .anchor-row").length),
    "premise: the empty render has no anchor rows").toBe(0);
  expect(
    await page.evaluate(() => document.getElementById("chat-scroll-container")!.clientHeight),
    "premise: the container is still laid out",
  ).toBeGreaterThan(0);
}

/// Wait for the app's ResizeObserver pass, then the following task and the
/// normal layout settling window.
export async function afterObserverAndTask(page: Page) {
  await page.evaluate(
    () =>
      new Promise<void>((resolve) => {
        const observer = new ResizeObserver(() => {
          observer.disconnect();
          setTimeout(resolve, 0);
        });
        observer.observe(document.getElementById("chat-content")!);
      }),
  );
  await afterLayoutSettles(page);
}

/// Where the history stood when its rows went away during a navigation.
export type EmptyAtRemoval = {
  /// `scrollTop`, the scroll range's inputs and the distance left to the
  /// click's destination, read once the rows are gone. For `render`, the
  /// history's height has collapsed by then, so `top` is the browser's clamp.
  top: number;
  scrollHeight: number;
  clientHeight: number;
  toDestination: number;
  /// Anchor rows rendered (0 for an empty history).
  rows: number;
  /// The last native `scroll` before the removal, and the browser's clamp
  /// (that minus `top`; 0 when the height was kept).
  lastScroll: number;
  clamp: number;
  endedBefore: boolean;
};

/// What `navigateWithEmptyHistory` saw.
export type NavigationEmpty = {
  start: number;
  destination: number;
  /// The newest visible message as the app last captured it before the rows
  /// went away: at the click, then at each native `scroll` or `end`.
  captured: { id: string; gap: number } | null;
  removal: EmptyAtRemoval;
  /// The first `scrollTop` write the app made after the removal (its stop),
  /// and where the view came to rest.
  firstWriteAfter: { pre: number; value: number } | null;
  rest: number;
  /// Every step on one clock: `click`, `scroll`, `end`, `emptied`, `write`
  /// (an app `scrollTop` write, `pre→value`), `observed` (the first
  /// ResizeObserver delivery after the removal) and `rest`.
  timeline: string;
};

/// Click scroll-to-latest and, in the click's own task, make the history
/// render no rows: `synthetic` swaps them for a placeholder of the same height
/// (`__riverEmptyRender`), `render` asks the app for an empty render
/// (`__riverTest.setHistoryEmpty`), which is deferred and collapses the height.
/// Everything is installed before the click and records on one clock. Resolves
/// once the rows are gone and no `scroll` has come for 400ms. The caller puts
/// the rows back (`restoreHistory`, or `setHistoryEmpty(false)`).
///
/// The click's own task, not a later `scroll`: the whole animation is a handful
/// of frames on CI's Linux WebKit (~1,800px in its first), so a removal at the
/// first `scroll` can leave the next frame landing on the destination, and the
/// controller would then have nothing left to stop.
export function navigateWithEmptyHistory(page: Page, how: "synthetic" | "render") {
  return page.evaluate(
    (how) =>
      new Promise<NavigationEmpty>((resolve, reject) => {
        const c = document.getElementById("chat-scroll-container")!;
        const content = document.getElementById("chat-content")!;
        const geometry = window.__riverHistoryGeometry!;
        const start = c.scrollTop;
        const destination = c.scrollHeight - c.clientHeight;
        const t0 = performance.now();
        const timeline: string[] = [];
        const native = Object.getOwnPropertyDescriptor(Element.prototype, "scrollTop")!;
        const top = () => native.get!.call(c) as number;
        const log = (kind: string) =>
          timeline.push(`${kind}@${top().toFixed(1)}+${(performance.now() - t0).toFixed(0)}ms`);
        const rowCount = () => content.querySelectorAll("[data-anchor-row]").length;
        let captured = geometry.newestVisible(c);
        let lastScroll = start;
        let endedBefore = false;
        let removal: EmptyAtRemoval | null = null;
        let firstWriteAfter: { pre: number; value: number } | null = null;
        let quiet = 0;
        const cleanup = () => {
          clearTimeout(timer);
          clearTimeout(quiet);
          observer.disconnect();
          resize.disconnect();
          c.removeEventListener("scroll", onScroll);
          c.removeEventListener("scrollend", onEnd);
          delete (c as unknown as { scrollTop?: number }).scrollTop;
        };
        const timer = setTimeout(() => {
          cleanup();
          reject(new Error(`navigation empty: the rows never went away and settled: ${timeline.join(" ")}`));
        }, 10_000);
        const settle = () => {
          clearTimeout(quiet);
          quiet = window.setTimeout(() => {
            log("rest");
            const rest = top();
            cleanup();
            resolve({
              start,
              destination,
              captured,
              removal: removal!,
              firstWriteAfter,
              rest,
              timeline: timeline.join(" "),
            });
          }, 400);
        };
        const noteRemoval = () => {
          if (removal || rowCount() > 0) return;
          const now = top();
          removal = {
            top: now,
            scrollHeight: c.scrollHeight,
            clientHeight: c.clientHeight,
            toDestination: destination - now,
            rows: rowCount(),
            lastScroll,
            clamp: lastScroll - now,
            endedBefore,
          };
          log("emptied");
          resize.observe(content);
          settle();
        };
        // Every position the app writes, recorded at the setter.
        Object.defineProperty(c, "scrollTop", {
          configurable: true,
          get: top,
          set: (value: number) => {
            const pre = top();
            native.set!.call(c, value);
            if (removal && !firstWriteAfter) firstWriteAfter = { pre, value };
            timeline.push(`write@${pre.toFixed(1)}→${value}+${(performance.now() - t0).toFixed(0)}ms`);
          },
        });
        let observedOnce = false;
        const resize = new ResizeObserver(() => {
          if (observedOnce) return;
          observedOnce = true;
          log("observed");
        });
        const observer = new MutationObserver(noteRemoval);
        observer.observe(content, { childList: true, subtree: true });
        // After the app's own listeners: the app has had each event.
        const onScroll = () => {
          log("scroll");
          if (removal) return settle();
          lastScroll = top();
          captured = geometry.newestVisible(c);
        };
        const onEnd = () => {
          log("end");
          if (removal) return settle();
          endedBefore = true;
          captured = geometry.newestVisible(c);
        };
        c.addEventListener("scroll", onScroll);
        c.addEventListener("scrollend", onEnd);
        log("click");
        document.querySelector<HTMLElement>('[data-testid="scroll-to-bottom"]')!.click();
        if (how === "synthetic") {
          window.__riverEmptyRender!.empty(true);
          noteRemoval();
        } else {
          void window.__riverTest!.setHistoryEmpty(true);
        }
      }),
    how,
  );
}
