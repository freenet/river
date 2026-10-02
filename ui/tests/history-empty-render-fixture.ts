import { expect, Page } from "@playwright/test";
import type { RowPosition } from "./history-scroll-geometry";
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

/// Read the visible message saved by the history geometry fixture.
export async function visibleRow(page: Page, why = "premise: a message should be visible"): Promise<RowPosition> {
  const row = await page.evaluate(() =>
    window.__riverHistoryGeometry!.newestVisible(document.getElementById("chat-scroll-container")!),
  );
  expect(row, why).not.toBeNull();
  return row!;
}
