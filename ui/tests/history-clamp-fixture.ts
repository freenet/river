import { expect, Page } from "@playwright/test";

/// The numbers HistoryScroll's layout signature describes (content and
/// container sizes), plus the scroll position and range, which it does not.
export type Shape = {
  top: number;
  max: number;
  contentHeight: number;
  contentWidth: number;
  clientHeight: number;
  clientWidth: number;
};

type ClampRecord = {
  before: Shape;
  after: Shape;
  scrolls: number;
};

declare global {
  interface Window {
    __historyClamp: {
      snapshot(): Shape;
      setHeight(height: number): { before: Shape; after: Shape };
      remove(): ClampRecord;
      removed: ClampRecord | null;
      scrolls: number;
      cleanup(): void;
    };
  }
}

// A clamp to a shorter range that no layout signature records. Geometry only:
// an absolute overhang below the history's content changes the live scroll
// range without changing any dimension in HistoryScroll's LayoutSig, the way
// an open popover near the end does, and removing it makes the browser clamp
// `scrollTop` by more than LAYOUT_SHIFT_ALLOWANCE_PX (history_scroll.rs).
// Installed after startup, so its scroll counter observes events after the
// app's own listener. Every read is synchronous, so a clamp has already
// happened by the `after` shape.
export function clampOverhang(page: Page, height = 700) {
  return page.evaluate((height) => {
    const c = document.getElementById("chat-scroll-container")!;
    const content = document.getElementById("chat-content")!;
    const shape = (): Shape => ({
      top: c.scrollTop,
      max: c.scrollHeight - c.clientHeight,
      contentHeight: content.clientHeight,
      contentWidth: content.clientWidth,
      clientHeight: c.clientHeight,
      clientWidth: c.clientWidth,
    });
    const before = shape();
    const position = content.style.position;
    content.style.position = "relative";
    const box = document.createElement("div");
    box.style.cssText = `position:absolute;top:100%;left:0;width:1px;height:${height}px;pointer-events:none;`;
    content.appendChild(box);
    const onScroll = () => window.__historyClamp.scrolls++;
    c.addEventListener("scroll", onScroll);
    window.__historyClamp = {
      removed: null,
      scrolls: 0,
      snapshot: shape,
      setHeight(height) {
        const before = shape();
        box.style.height = `${height}px`;
        return { before, after: shape() };
      },
      remove() {
        const before = shape();
        box.remove();
        return (this.removed = { before, after: shape(), scrolls: this.scrolls });
      },
      cleanup() {
        c.removeEventListener("scroll", onScroll);
        box.remove();
        content.style.position = position;
      },
    };
    return { before, after: shape() };
  }, height);
}

/// Resize the installed overhang.
export function resizeOverhang(page: Page, height: number) {
  return page.evaluate((height) => window.__historyClamp.setHeight(height), height);
}

/// Remove the installed overhang; the browser clamps at once.
export function removeOverhang(page: Page): Promise<ClampRecord> {
  return page.evaluate(() => window.__historyClamp.remove());
}

export function expectFinalEndClamp(record: ClampRecord) {
  const { before, after } = record;
  for (const key of ["contentHeight", "contentWidth", "clientHeight", "clientWidth"] as const) {
    expect(after[key], `premise: the clamp does not change ${key}`).toBe(before[key]);
  }
  expect(before.top - after.top, "premise: the clamp exceeds the 200px layout allowance").toBeGreaterThan(200);
  expect(Math.abs(after.max - after.top), `premise: the clamp lands at the live end (${JSON.stringify(record)})`).toBeLessThanOrEqual(1);
  expect(before.top, "premise: the saved position cannot be restored in the shortened range").toBeGreaterThan(after.max + 4);
}

export async function clampCleanup(page: Page) {
  await page.evaluate(() => window.__historyClamp?.cleanup());
}
