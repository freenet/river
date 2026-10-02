import { expect, Page } from "@playwright/test";

// Browser-test utilities for removing EVERY row the history remembers as the
// reader's anchor while their gesture is still open, so a restore finds no
// saved row (history_scroll.rs, `restore_anchor`), and for checking that the
// removal is the one the regression claims. Shared by the native-end and mobile
// reveal cases in conversation-follow-state.spec.ts and the no-`scrollend`
// fallback case in conversation-scroll-debounce.spec.ts.
//
// The neighbourhood is the reader's newest visible row and every anchor-bearing
// row from two viewports above it, and never fewer than eight rows above it: far
// more than the handful of fallbacks the model remembers, without mirroring
// that number. It must hold plain inbound filler messages only (`filler N:`),
// so no date separator or event row survives as a fallback. Removing it takes
// away more height than the reader was parked above the end, so the browser
// clamps `scrollTop` to the new end: a reader who was outside the follow band is
// put inside it by layout alone.
//
// Removal goes through the app's `removeMessages` test hook, in one state
// change. The hook defers its write (`setTimeout(0)`), and every caller here
// runs on a paused Playwright clock, so the removal runs the clock for zero ms
// to let it happen; nothing else on the clock moves. Nothing here writes
// `scrollTop`, dispatches an event, or implements a follow policy. The in-page
// parts must be self-contained.

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
const BOTTOM_THRESHOLD_PX = 100;
/// The geometry budget for "the view did not move", as IN_PLACE_TOLERANCE_PX in
/// the specs.
const IN_PLACE_TOLERANCE_PX = 4;

/// The container's geometry: `scrollTop`, its height, the live end, and how far
/// that end is below the view.
export type MissingAnchorGeometry = {
  top: number;
  height: number;
  scrollHeight: number;
  max: number;
  distance: number;
};

/// The neighbourhood chosen right after the reader's last delivered move.
export type MissingAnchorSelection = {
  /// Row ids to remove (`msg-…`), oldest first.
  ids: string[];
  /// The newest visible row, the one the reader's capture saved first.
  newest: string;
  /// Anchor-bearing rows left above the neighbourhood.
  above: number;
  before: MissingAnchorGeometry;
};

/// What the removal did, read once the caller's frame wait has let the clamp's
/// `scroll` and the ResizeObserver reach the app.
export type MissingAnchorRemoval = {
  before: MissingAnchorGeometry;
  after: MissingAnchorGeometry;
  /// The container's `scroll` events since the removal was requested (counted
  /// after the app's listener), and `scrollTop` as the last one was delivered.
  scrolls: number;
  topAtLastScroll: number | null;
  /// Deliveries of this fixture's own observer on `#chat-content` that saw it at
  /// a new height. It is made after the app's, so the app's restore had already
  /// run for each.
  observed: number;
  /// Anchor-bearing rows remaining.
  remaining: number;
};

declare global {
  interface Window {
    __missingAnchor?: {
      scrolls: number;
      topAtLastScroll: number | null;
      observed: number;
      removal: Promise<string[]> | null;
      stop(): void;
    };
  }
}

function geometry(page: Page): Promise<MissingAnchorGeometry> {
  return page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const max = c.scrollHeight - c.clientHeight;
    return { top: c.scrollTop, height: c.clientHeight, scrollHeight: c.scrollHeight, max, distance: max - c.scrollTop };
  });
}

/// Choose the neighbourhood (see the module comment) from the anchor-bearing
/// rows as they are now. Call right after the reader's last delivered move, so
/// they are the rows that move's capture saved. Checks the premises that make
/// it the right neighbourhood: the reader is parked outside the band, and every
/// chosen row is a plain filler message with rows left above it.
export async function missingAnchorSelect(page: Page): Promise<MissingAnchorSelection> {
  const picked = await page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const box = c.getBoundingClientRect();
    const rows = Array.from(c.querySelectorAll<HTMLElement>("#chat-content [data-anchor-row]")).map((row) => {
      const r = row.getBoundingClientRect();
      return {
        id: row.id,
        text: (row.textContent ?? "").trim().slice(0, 40),
        top: r.top - box.top,
        bottom: r.bottom - box.top,
      };
    });
    const newestAt = rows.findLastIndex((row) => row.top < c.clientHeight && row.bottom > 0);
    const reach = rows.findIndex((row) => row.bottom > -2 * c.clientHeight);
    const firstAt = Math.min(reach, newestAt - 8);
    return { rows, newestAt, firstAt };
  });
  const before = await geometry(page);
  expect(picked.newestAt, "premise: a row is visible").toBeGreaterThanOrEqual(0);
  expect(before.distance, "premise: the reader is parked outside the follow band").toBeGreaterThan(BOTTOM_THRESHOLD_PX);
  expect(picked.firstAt, "premise: anchor-bearing rows remain above the neighbourhood").toBeGreaterThan(0);
  const doomed = picked.rows.slice(picked.firstAt, picked.newestAt + 1);
  expect(
    doomed.filter((row) => !(row.id.startsWith("msg-") && /^filler \d+:/.test(row.text))),
    "premise: the neighbourhood holds plain filler messages only, no separator or event row",
  ).toEqual([]);
  return {
    ids: doomed.map((row) => row.id),
    newest: picked.rows[picked.newestAt].id,
    above: picked.firstAt,
    before,
  };
}

/// Remove the selection in one state change on the paused clock, wait with
/// `frames` (the caller's real-frame wait) for the layout to reach the app, and
/// check that the removal did what the regressions need: every row gone and
/// none of them left anchor-bearing, the layout changed, `scrollTop` clamped
/// materially, scrollable content left, and the view now inside the follow
/// band. The clamped geometry is returned for the outcome checks, since no
/// saved row is left to compare against.
export async function missingAnchorRemove(
  page: Page,
  selection: MissingAnchorSelection,
  frames: () => Promise<void>,
): Promise<MissingAnchorRemoval> {
  const before = await geometry(page);
  await page.evaluate((ids) => {
    const c = document.getElementById("chat-scroll-container")!;
    const content = document.getElementById("chat-content")!;
    const record = {
      scrolls: 0,
      topAtLastScroll: null as number | null,
      observed: 0,
      removal: null as Promise<string[]> | null,
      stop() {},
    };
    // Added after the app's listener and observer, so each runs once the app
    // has had its own.
    const onScroll = () => {
      record.scrolls++;
      record.topAtLastScroll = c.scrollTop;
    };
    c.addEventListener("scroll", onScroll);
    // Its initial delivery reports the size as observed, not a change: count
    // only deliveries that see the content at another height.
    const observedHeight = content.clientHeight;
    const observer = new ResizeObserver(() => {
      if (content.clientHeight !== observedHeight) record.observed++;
    });
    observer.observe(content);
    record.stop = () => {
      c.removeEventListener("scroll", onScroll);
      observer.disconnect();
    };
    window.__missingAnchor = record;
    const hooks = window.__riverTest;
    if (!hooks) throw new Error("missing anchor: window.__riverTest is not available in this build");
    record.removal = hooks.removeMessages(ids);
  }, selection.ids);
  try {
    await page.clock.runFor(0);
    const unmatched = await page.evaluate(() => window.__missingAnchor!.removal!);
    expect(unmatched, "premise: every id named a message").toEqual([]);
    await expect
      .poll(() => page.evaluate((ids) => ids.filter((id) => document.getElementById(id)).length, selection.ids), {
        timeout: 5_000,
        message: "premise: every removed message left the page",
      })
      .toBe(0);
    await frames();
    const seen = await page.evaluate(() => {
      const rec = window.__missingAnchor!;
      const remaining = document.querySelectorAll("#chat-content [data-anchor-row]").length;
      return { scrolls: rec.scrolls, topAtLastScroll: rec.topAtLastScroll, observed: rec.observed, remaining };
    });
    const after = await geometry(page);
    const removal: MissingAnchorRemoval = { before, after, ...seen };
    const what = JSON.stringify(removal);
    expect(after.height, `premise: the history is laid out (${what})`).toBeGreaterThan(0);
    expect(
      before.scrollHeight - after.scrollHeight,
      `premise: the removal took away more than the reader was parked above the end (${what})`,
    ).toBeGreaterThan(before.distance);
    expect(before.top - after.top, `premise: the browser clamped scrollTop materially (${what})`).toBeGreaterThan(
      BOTTOM_THRESHOLD_PX,
    );
    expect(seen.remaining, `premise: anchor-bearing rows remain (${what})`).toBeGreaterThan(0);
    expect(after.max, `premise: the remaining history still scrolls (${what})`).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    expect(after.distance, `premise: the clamp put the view inside the follow band (${what})`).toBeLessThanOrEqual(
      BOTTOM_THRESHOLD_PX - IN_PLACE_TOLERANCE_PX,
    );
    expect(seen.scrolls, `premise: the clamp's scroll reached the app (${what})`).toBeGreaterThan(0);
    expect(seen.observed, `premise: the ResizeObserver reported the removal to the app (${what})`).toBeGreaterThan(0);
    return removal;
  } finally {
    await page.evaluate(() => {
      window.__missingAnchor?.stop();
      delete window.__missingAnchor;
    });
  }
}

/// How far `scrollTop` is from `top`, the post-removal offset.
export async function missingAnchorDrift(page: Page, top: number): Promise<number> {
  return Math.abs((await geometry(page)).top - top);
}

/// Hide the history's scrollbar for the page's life, from before the app
/// starts. Call before the first navigation.
///
/// For the reveal cases. WebKit's history has a classic 8px scrollbar, which a
/// revealed container does not have yet at the first layout that puts its saved
/// offset back: the rows wrap at the wider width, the offset is clamped to that
/// shorter range, and the scrollbar then narrows the rows again and leaves the
/// view a few wrapped lines above the end (40 to 120px across runs). With a
/// saved row, the app's restore puts that right; with none left, nothing does,
/// so the reader's offset after the reveal would be the engine's, sometimes
/// outside the band the regression needs. Without a scrollbar, the width never
/// changes. Chromium and Firefox here draw overlay scrollbars, so nothing
/// changes for them.
export async function missingAnchorHideScrollbar(page: Page) {
  await page.addInitScript(() => {
    const css =
      "#chat-scroll-container{scrollbar-width:none}#chat-scroll-container::-webkit-scrollbar{display:none}";
    const add = () => {
      const style = document.createElement("style");
      style.dataset.missingAnchor = "hide-scrollbar";
      style.textContent = css;
      document.head.appendChild(style);
    };
    if (document.head) add();
    else document.addEventListener("DOMContentLoaded", add, { once: true });
  });
}
