import { Page } from "@playwright/test";

/// A message row and the gap from the scroll container's bottom edge to its top.
export type RowPosition = { id: string; gap: number };

/// A saved row's gap, plus whether any part of it intersects the viewport.
type SavedRow = RowPosition & { visible: boolean };

type HistoryGeometry = {
  newestVisible(container: HTMLElement): RowPosition | null;
  positionOf(container: HTMLElement, id: string): SavedRow | null;
};

declare global {
  interface Window {
    __riverHistoryGeometry?: HistoryGeometry;
  }
}

/// Install `window.__riverHistoryGeometry`. Self-contained, so it can be an
/// init script or run in the current document. A second install leaves the
/// first in place.
function installHistoryGeometry() {
  const current = window.__riverHistoryGeometry;
  if (typeof current?.newestVisible === "function" && typeof current?.positionOf === "function") return;
  const missingContainer = "history geometry: chat scroll container is missing";
  const intersecting = (box: DOMRect, row: DOMRect) => row.bottom > box.top && row.top < box.bottom;
  window.__riverHistoryGeometry = {
    newestVisible(container) {
      if (!container) throw new Error(missingContainer);
      const box = container.getBoundingClientRect();
      let found: RowPosition | null = null;
      for (const row of container.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
        const rect = row.getBoundingClientRect();
        if (intersecting(box, rect)) found = { id: row.id, gap: box.bottom - rect.top };
      }
      return found;
    },
    positionOf(container, id) {
      if (!container) throw new Error(missingContainer);
      const row = document.getElementById(id);
      if (!row || !container.contains(row)) return null;
      const box = container.getBoundingClientRect();
      const rect = row.getBoundingClientRect();
      return { id: row.id, gap: box.bottom - rect.top, visible: intersecting(box, rect) };
    },
  };
}

/// Register the installer for every later navigation. Does not read the
/// document, so it is independent of other init scripts.
export function registerHistoryGeometry(page: Page): Promise<void> {
  return page.addInitScript(installHistoryGeometry);
}

function readNewest(page: Page): Promise<RowPosition | null> {
  return page.evaluate(() => {
    const geo = window.__riverHistoryGeometry;
    if (!geo) throw new Error("history geometry is not installed");
    const container = document.getElementById("chat-scroll-container");
    if (!container) throw new Error("history geometry: chat scroll container is missing");
    return geo.newestVisible(container);
  });
}

function readSaved(page: Page, id: string): Promise<SavedRow | null> {
  return page.evaluate((id) => {
    const geo = window.__riverHistoryGeometry;
    if (!geo) throw new Error("history geometry is not installed");
    const container = document.getElementById("chat-scroll-container");
    if (!container) throw new Error("history geometry: chat scroll container is missing");
    return geo.positionOf(container, id);
  }, id);
}

/// The newest message intersecting the viewport, or null when none does.
export function newestVisibleRow(page: Page): Promise<RowPosition | null> {
  return readNewest(page);
}

/// Drift of the saved row while it is still the newest visible message.
/// Missing, replaced, or no longer newest is Infinity, never zero.
export async function newestMessageDrift(page: Page, before: RowPosition): Promise<number> {
  const now = await readNewest(page);
  return now?.id === before.id ? Math.abs(now.gap - before.gap) : Infinity;
}

/// Drift of the saved row's gap. A newer message may be visible, and the row
/// may sit outside the viewport. Missing or outside the container is Infinity.
export async function savedRowDrift(page: Page, before: RowPosition): Promise<number> {
  const now = await readSaved(page, before.id);
  return now ? Math.abs(now.gap - before.gap) : Infinity;
}

/// Drift of the saved row while any part of it stays in view. A newer row may
/// enter the viewport. Hidden, missing, or outside the container is Infinity.
export async function savedVisibleRowDrift(page: Page, before: RowPosition): Promise<number> {
  const now = await readSaved(page, before.id);
  return now?.visible ? Math.abs(now.gap - before.gap) : Infinity;
}
