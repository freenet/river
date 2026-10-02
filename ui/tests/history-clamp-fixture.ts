import { expect, Page } from "@playwright/test";

type Shape = {
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
      remove(): ClampRecord;
      removed: ClampRecord | null;
      scrolls: number;
      cleanup(): void;
    };
  }
}

// Geometry only: an absolute overhang changes the live scroll range without
// changing any dimension in HistoryScroll's LayoutSig. Installed after startup,
// so its scroll counter observes events after the app's own listener.
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

// An arrival can fill the blank overhang area with a newer visible row while
// the saved row stays exactly in place. Compare the saved identity and gap,
// rather than requiring it to remain the newest visible message.
export function clampRowDrift(page: Page, at: { id: string; gap: number }) {
  return page.evaluate(({ id, gap }) => {
    const c = document.getElementById("chat-scroll-container")!;
    const row = document.getElementById(id);
    if (!row || !c.contains(row)) return Infinity;
    const view = c.getBoundingClientRect();
    const rect = row.getBoundingClientRect();
    if (rect.bottom <= view.top || rect.top >= view.bottom) return Infinity;
    return Math.abs(view.bottom - rect.top - gap);
  }, at);
}
