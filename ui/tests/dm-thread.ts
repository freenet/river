import { expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { DM_VIEW, expectSettledAtBottom, readerScrollsWithoutGesture, scrollMetrics } from "./history-geometry";

// Helpers shared by the DM thread specs. The room's geometry helpers in
// history-geometry.ts serve the thread too, given `DM_VIEW`.

/// test_hooks.rs `DM_PEERS`: index, nickname, and the label of the history
/// `appendDmsForPeer` seeds ("<history> NN").
export const DM_PEER = { index: 0, name: "DM Test Peer", history: "dm history" } as const;
export const OTHER_DM_PEER = { index: 1, name: "Other DM Peer", history: "other dm history" } as const;
export type DmPeer = typeof DM_PEER | typeof OTHER_DM_PEER;

/// DMs seeded per thread, enough to scroll on every test viewport.
export const HISTORY = 30;
export const THREAD = "#dm-scroll-container";
export const LATEST = "dm-scroll-to-latest";
/// How far a view that must not move may still drift (subpixel rounding).
export const STILL_PX = 1;

/// The newest of `count` DMs `appendDmsForPeer` seeded for `peer`.
export function newestHistoryDm(peer: DmPeer, count = HISTORY) {
  return `${peer.history} ${String(count - 1).padStart(2, "0")}`;
}

function railRow(page: Page, peer: DmPeer) {
  return page.locator(".dm-rail-row-btn", { hasText: peer.name });
}

/// The thread's unread count in the DM rail. In the DOM even while the modal
/// covers it, and on phones while the rooms panel is shut.
export function railBadge(page: Page, peer: DmPeer = DM_PEER) {
  return railRow(page, peer).getByTestId("dm-rail-unread-badge");
}

/// Open `peer`'s thread from its rail row, and wait for its `newest` DM to
/// render when given.
export async function openThread(page: Page, peer: DmPeer = DM_PEER, newest?: string) {
  const row = railRow(page, peer);
  // Phone widths keep the DM rail in the rooms panel, behind the hamburger.
  const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
  if (!(await row.isVisible()) && (await hamburger.count()) > 0) await hamburger.click();
  await row.click();
  await expect(page.getByTestId("dm-thread-modal").locator("h2")).toContainText(peer.name, { timeout: 5_000 });
  await expect(page.locator(THREAD)).toBeVisible({ timeout: 5_000 });
  if (newest !== undefined) await expect(dm(page, newest)).toHaveCount(1, { timeout: 5_000 });
}

export async function closeThread(page: Page) {
  await page.getByTestId("dm-thread-close-button").click();
  await expect(page.getByTestId("dm-thread-modal")).toHaveCount(0);
}

export function dm(page: Page, text: string) {
  return page.locator(THREAD).getByText(text, { exact: true });
}

/// Deliver an inbound DM from `peer` and wait for it to render in the open
/// thread. Attached, not visible: it may land below the view.
export async function deliverDm(page: Page, text: string, peer: DmPeer = DM_PEER) {
  await callRiverTest(page, "deliverDmForPeer", peer.index, text);
  await expect(dm(page, text)).toHaveCount(1, { timeout: 5_000 });
}

/// The open thread settles at its end.
export function expectAtEnd(page: Page, why: string) {
  return expectSettledAtBottom(page, why, DM_VIEW);
}

/// The reader scrolls the open thread to `top` and the move lands. Native
/// scrolling: the thread has no gesture handling for a wheel or touch event to
/// change.
export async function scrollThreadTo(page: Page, top: number) {
  await readerScrollsWithoutGesture(page, top, DM_VIEW);
}

/// Record every position the open thread passes through from now on.
export async function recordScrolls(page: Page) {
  await page.evaluate((sel) => {
    const el = document.querySelector(sel)!;
    const log = { start: el.scrollTop, seen: [] as number[] };
    (window as unknown as { __dmScrolls: typeof log }).__dmScrolls = log;
    el.addEventListener("scroll", () => log.seen.push(el.scrollTop));
  }, THREAD);
}

export function recordedScrolls(page: Page) {
  return page.evaluate(
    () => (window as unknown as { __dmScrolls: { start: number; seen: number[] } }).__dmScrolls,
  );
}

/// Wait `holdMs`, then assert the open thread never left where it was when
/// `recordScrolls` started, not even in passing. `soft` lets the assertions
/// after it still report.
export async function expectStill(page: Page, why: string, { soft = false, holdMs = 600 } = {}) {
  if (holdMs > 0) await page.waitForTimeout(holdMs);
  const { start, seen } = await recordedScrolls(page);
  const { top } = await scrollMetrics(page, DM_VIEW);
  const furthest = Math.max(0, ...[...seen, top].map((t) => Math.abs(t - start)));
  (soft ? expect.soft : expect)(furthest, why).toBeLessThanOrEqual(STILL_PX);
}

/// Uncaught page errors and console errors from now on: a WASM panic or a
/// RefCell borrow error surfaces as either.
export function recordPageErrors(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  return errors;
}
