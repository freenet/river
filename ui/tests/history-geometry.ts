import { expect, Page, Route, test } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom, nextFrames, settle } from "./example-room";

export { nextFrames, settle };

// Geometry and event-order helpers shared by the conversation scroll specs.
// Rows are identified by `data-item-key` (a group's first message id), never
// by Dioxus render-order ids.

/// Rendered history rows: display items plus date separators.
export const HISTORY_ROWS = '[data-testid="conversation-history"] > *';

/// A premise distance: a view this far from the end of the history is well
/// away from it.
export const WELL_AWAY_FROM_END_PX = 100;
/// scroll_to_latest.rs `NEWEST_IN_VIEW_SLACK_PX`, shared by the room and the
/// DM thread: how far below the visible area the newest message's bottom may
/// sit and still count as on screen.
export const NEWEST_IN_VIEW_SLACK_PX = 4;
/// Slack for fractional layout after a scroll that did land at the bottom.
export const AT_BOTTOM_EPSILON_PX = 4;
/// scroll_to_latest.rs `FOLLOW_BAND_PX`, shared by the room and the DM thread:
/// how far below the view the newest message's bottom may sit when an arrival
/// lands and still be followed.
export const FOLLOW_BAND_PX = 100;
/// The band's edge, tested once per surface: a reader parked this far inside
/// the band follows, and this far outside it does not.
export const BAND_EDGES = [
  [-20, true],
  [20, false],
] as const;
/// How far a row the reader is looking at may move and still count as "kept
/// in place". Independent of the app's own slack (#732).
export const READING_ROW_BUDGET_PX = 4;

/// The main SHA the known failures below were reproduced on, before the scroll simplification.
const KNOWN_FAILURE_SHA = "1ce80050";

/// Mark the REST of the test as a reproduced, known failure on main.
///
/// Call it after the scenario and its premises are established, immediately
/// before the defect assertion, so a broken setup still fails normally. Listed
/// per project: each was verified failing there. An unexpected pass fails the
/// run and means the annotation has to go.
export function knownFailure(projects: readonly string[], reference: string) {
  test.fail(
    projects.includes(test.info().project.name),
    `known failure on main ${KNOWN_FAILURE_SHA}: ${reference}`,
  );
}

/// Every browser project in playwright.config.ts.
export const ALL_PROJECTS = ["chromium", "firefox", "webkit", "mobile-chrome", "mobile-safari"] as const;

/// Which scroller a geometry helper reads: the room history or a DM thread,
/// each with the sentinel that marks its newest item's bottom edge. They settle
/// differently: the room waits for `scrollend`, which its own settle logic
/// listens for; the DM thread has no settle logic, so a `scroll` and the frame
/// after it are enough.
type HistoryView = { container: string; sentinel: string; settle: "scrollend" | "frame" };
const ROOM_VIEW: HistoryView = {
  container: "chat-scroll-container",
  sentinel: "bottom-sentinel",
  settle: "scrollend",
};
export const DM_VIEW: HistoryView = {
  container: "dm-scroll-container",
  sentinel: "dm-bottom-sentinel",
  settle: "frame",
};

/// Where the view is scrolled (`top`, at most `max`), the history's total
/// `height`, the height of the window onto it (`client`, which shrinks when the
/// composer grows), and `toEnd`: how far the end of the history is below the
/// visible area, 0 when the newest item is fully in view. NaN when the
/// scroller is not in the DOM.
export function scrollMetrics(page: Page, view: HistoryView = ROOM_VIEW) {
  return page.evaluate((id) => {
    const el = document.getElementById(id);
    if (!el) return { top: NaN, height: NaN, client: NaN, max: NaN, toEnd: NaN };
    return {
      top: el.scrollTop,
      height: el.scrollHeight,
      client: el.clientHeight,
      max: el.scrollHeight - el.clientHeight,
      toEnd: el.scrollHeight - el.scrollTop - el.clientHeight,
    };
  }, view.container);
}

/// `scrollMetrics().toEnd`, the measure most polls wait on.
export async function distanceFromBottom(page: Page, view: HistoryView = ROOM_VIEW): Promise<number> {
  return (await scrollMetrics(page, view)).toEnd;
}

/// How many newer items the rendered range is holding back, from the
/// history's `data-newer-withheld`.
export function withheld(page: Page): Promise<number> {
  return page
    .getByTestId("conversation-history")
    .getAttribute("data-newer-withheld")
    .then((n) => Number(n));
}

/// Premise shared by the parked-reader tests: the view is well away from the
/// end of the history.
export async function expectParkedAwayFromEnd(page: Page, why = "premise: parked away from the end") {
  expect(await distanceFromBottom(page), why).toBeGreaterThan(WELL_AWAY_FROM_END_PX);
}

export async function expectSettledAtBottom(page: Page, why: string, view: HistoryView = ROOM_VIEW) {
  await expect
    .poll(() => distanceFromBottom(page, view), { timeout: 5_000, message: why })
    .toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
}

/// Deliver an inbound message, exactly as an arriving network update does, and
/// wait until it is actually on the page.
///
/// Waiting on the text rather than on a fixed delay matters for the tests that
/// assert the view did NOT move: a timeout that expires before the render lands
/// passes whether or not the bug is present.
export async function deliver(page: Page, text: string) {
  await callRiverTest(page, "appendMessage", text);
  await expect(page.getByText(text, { exact: false }).last()).toBeVisible({
    timeout: 5_000,
  });
}

/// Like `deliver`, but only waits for the patch: the arrival may land below a
/// parked reader's viewport, where `toBeVisible` would still pass but says
/// nothing about where the view is.
export async function deliverOffscreen(page: Page, text: string) {
  await callRiverTest(page, "appendMessage", text);
  await expect(page.getByText(text, { exact: false })).toHaveCount(1, { timeout: 5_000 });
}

/// Open a room and wait until the history has settled at its newest message.
///
/// `path` opts into fixture variants the default build hides
/// (`/?deep-history-room=1`, `/?uneven-history=1`).
export async function openRoomAtBottom(page: Page, roomName: string, path = "/") {
  await page.goto(path);
  await waitForApp(page);
  await selectRoom(page, roomName);
  // Off the history: a pointer left over it by the room-list click expands any
  // reply strip it hovers (main.css), and that reflow is not followed.
  await page.mouse.move(0, 0);
  // Asserted since a hidden panel would make every geometry read return 0.
  await expect(page.locator("#chat-scroll-container")).toBeVisible({ timeout: 5_000 });
  await expectSettledAtBottom(page, "opening a room should land on its newest message");
}

/// The reader scrolls the history to `top`. Assigning `scrollTop` moves the
/// viewport on every engine (`page.mouse.wheel` is unsupported on mobile
/// WebKit); the app has no wheel, pointer or touch handling for a gesture event
/// to change. Returns without waiting for the settle, so callers can act
/// before it lands.
export async function readerScrollsTo(page: Page, top: number) {
  await page.evaluate((t) => {
    document.getElementById("chat-scroll-container")!.scrollTop = t;
  }, top);
}

/// The same, waiting for the settle, so the app has seen the move end before
/// the caller acts. Tests that need an un-settled move use `readerScrollsTo`.
export async function readerScrollsWithoutGesture(page: Page, top: number, view: HistoryView = ROOM_VIEW) {
  await scrollAndSettle(page, top, view);
}

/// Move the history to `target` (or its end) with no gesture event, and wait
/// for the move to settle as `view.settle` says. A target the container clamps
/// to where it already is scrolls nothing, so there is no settle to wait for.
///
/// The listener goes on before the move, and the move and the wait share one
/// `evaluate`, so a settle that lands quickly cannot be missed. `"end"` is
/// resolved in the page, from the history's height at the moment of the move.
async function scrollAndSettle(page: Page, target: number | "end", view: HistoryView = ROOM_VIEW) {
  await page.evaluate(
    async ({ id, settle, t }) => {
      const el = document.getElementById(id)!;
      const event = settle === "scrollend" ? "scrollend" : "scroll";
      let timer: ReturnType<typeof setTimeout> | undefined;
      let onSettle: (() => void) | undefined;
      const settled = new Promise<void>((resolve, reject) => {
        onSettle = settle === "scrollend" ? resolve : () => requestAnimationFrame(() => resolve());
        timer = setTimeout(() => reject(new Error("the scroll never settled")), 5_000);
        el.addEventListener(event, onSettle);
      });
      try {
        const before = el.scrollTop;
        el.scrollTop = t === "end" ? el.scrollHeight : t;
        // Unmoved: no settle is coming, and the timer must not reject a
        // promise nobody awaits.
        if (el.scrollTop !== before) await settled;
      } finally {
        clearTimeout(timer);
        el.removeEventListener(event, onSettle!);
      }
    },
    { id: view.container, settle: view.settle, t: target },
  );
}

/// Hold for a moment and assert the view did not move.
///
/// Compares `scrollTop` rather than distance-from-bottom: distance also moves
/// when content grows, so it would tolerate a partial yank of up to the new
/// message's height.
export async function expectStaysPut(page: Page, why: string) {
  const before = (await scrollMetrics(page)).top;
  await page.waitForTimeout(600);
  expect((await scrollMetrics(page)).top, why).toBeCloseTo(before, 0);
}

/// Add enough history to have somewhere to scroll back through, and leave the
/// reader at its end.
///
/// The reader goes to the end themselves rather than relying on the arrivals
/// being followed, so this premise holds whether or not arrivals move the view.
export async function fillHistory(page: Page, label = "filler") {
  for (let i = 0; i < 8; i++) {
    await deliver(page, `${label} ${i}: ${"y".repeat(200)}`);
  }
  await readerReturnsToEnd(page);
  await expectSettledAtBottom(page, "the reader should be at the end after the fillers");
}

/// The reader scrolls to the end of the history with no gesture event, and the
/// app sees the move settle. Already at the end, nothing scrolls and there is
/// no settle to wait for.
export async function readerReturnsToEnd(page: Page) {
  await scrollAndSettle(page, "end");
}

/// The reader scrolls a little way up from the end and stays there. The end
/// hold, which a request or a scroll settling at the end arms, is released;
/// the newest message is still within the follow band.
export async function readerLeavesTheEnd(page: Page) {
  await readerScrollsWithoutGesture(page, (await scrollMetrics(page)).top - 60);
  expect(await distanceFromBottom(page), "premise: the reader left the end").toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
}

/// The row the reader is looking at: an item row fully inside the visible part
/// of the history, with the preceding row's key for deletion cases.
type ReadingRow = {
  key: string;
  top: number;
  text: string;
  prevKey: string | null;
};

/// The first item row fully inside the container's visible area, optionally
/// the first one whose text contains `containing`.
export async function readingRow(page: Page, containing?: string): Promise<ReadingRow | null> {
  return page.evaluate((needle) => {
    const c = document.getElementById("chat-scroll-container")!;
    const cRect = c.getBoundingClientRect();
    const rows = Array.from(c.querySelectorAll<HTMLElement>("[data-item-key]"));
    for (let i = 0; i < rows.length; i++) {
      const r = rows[i].getBoundingClientRect();
      if (r.height === 0 || r.top < cRect.top || r.bottom > cRect.bottom) continue;
      const text = rows[i].textContent ?? "";
      if (needle && !text.includes(needle)) continue;
      return {
        key: rows[i].getAttribute("data-item-key")!,
        top: r.top - cRect.top,
        text,
        prevKey: rows[i - 1]?.getAttribute("data-item-key") ?? null,
      };
    }
    return null;
  }, containing ?? null);
}

/// `readingRow`, asserted as a premise: some row has to be fully in view for
/// "the view did not move" to be measurable.
export async function expectReadingRow(page: Page, containing?: string): Promise<ReadingRow> {
  const row = await readingRow(page, containing);
  expect(row, "premise: a row is fully in view").not.toBeNull();
  return row!;
}

/// The message whose text contains `text` is entirely inside the visible
/// history, so its bottom is above the composer.
export async function expectMessageInView(page: Page, text: string, why: string) {
  const message = page.locator("[data-anchor-key]", { hasText: text });
  await expect(message, `${why} (the message is not rendered)`).toHaveCount(1);
  const overflow = await message.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const c = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return { above: c.top - r.top, below: r.bottom - c.bottom };
  });
  expect(overflow.above, `${why} (its top is ${overflow.above}px above the view)`).toBeLessThanOrEqual(1);
  expect(overflow.below, `${why} (its bottom is ${overflow.below}px below the view)`).toBeLessThanOrEqual(1);
}

/// An arrival was followed: the history settled at its end with the message
/// entirely in view.
export async function expectFollowed(page: Page, text: string, why: string) {
  await expectSettledAtBottom(page, why);
  await expectMessageInView(page, text, why);
}

/// How far the newest item's bottom (`view.sentinel`'s top, the edge the
/// follow rule measures) sits below the visible history. Negative: above its
/// bottom edge.
export function newestBelowView(page: Page, view: HistoryView = ROOM_VIEW): Promise<number> {
  return page.evaluate(({ container, sentinel }) => {
    const c = document.getElementById(container)!;
    const s = document.getElementById(sentinel)!;
    return s.getBoundingClientRect().top - c.getBoundingClientRect().bottom;
  }, view);
}

/// The reader scrolls so the newest item's bottom sits `below` px under the
/// view's bottom edge, and the move settles.
export async function readerParksNewestBelow(page: Page, below: number, view: HistoryView = ROOM_VIEW) {
  const target = (await scrollMetrics(page, view)).top + (await newestBelowView(page, view)) - below;
  await readerScrollsWithoutGesture(page, target, view);
  expect(Math.abs((await newestBelowView(page, view)) - below), "premise: parked where asked").toBeLessThanOrEqual(2);
}

/// Which edge of a row to measure.
type RowEdge = "top" | "bottom";

/// How a row's place is measured: `"top"` is its top below the view's top
/// edge; `{ fromBottom }` is how far that edge of it sits above the view's
/// bottom edge, which is what a row keeps when the chat area changes height.
type RowMeasure = "top" | { fromBottom: RowEdge };

/// Where row `key` sits by `measure`, or null when it is not in the DOM
/// (deleted from the data, or evicted from the rendered range).
export function rowOffset(page: Page, key: string, measure: RowMeasure = "top"): Promise<number | null> {
  return page.evaluate(
    ({ k, m }) => {
      const c = document.getElementById("chat-scroll-container")!;
      const row = c.querySelector(`[data-item-key="${CSS.escape(k)}"]`);
      if (!row) return null;
      const view = c.getBoundingClientRect();
      const r = row.getBoundingClientRect();
      return m === "top" ? r.top - view.top : view.bottom - r[m.fromBottom];
    },
    { k: key, m: measure },
  );
}

/// The newest row, with how far its `edge` sits above the view's bottom edge.
export async function newestRowFromViewBottom(
  page: Page,
  edge: RowEdge = "bottom",
): Promise<{ key: string; gap: number }> {
  const key = await page.locator("[data-item-key]").last().getAttribute("data-item-key");
  expect(key, "premise: the history has rows").not.toBeNull();
  return { key: key!, gap: (await rowOffset(page, key!, { fromBottom: edge }))! };
}

/// Measure `key` by `measure` after the patch has settled and again a moment
/// later, so a late correction (or a late yank) counts against the row too.
/// With `{ fromBottom }`, the row keeps its distance from the bottom of the
/// view when the chat area changes height.
export async function expectRowHeld(
  page: Page,
  key: string,
  expected: number,
  why: string,
  measure: RowMeasure = "top",
) {
  for (let i = 0; i < 2; i++) {
    await settle(page);
    const at = await rowOffset(page, key, measure);
    expect(at, `${why} (the row left the DOM)`).not.toBeNull();
    // Positive: the row moved down.
    const moved = measure === "top" ? at! - expected : expected - at!;
    expect(Math.abs(moved), `${why} (moved ${moved}px)`).toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
  }
}

/// Below the 768px breakpoint, open the members panel, which replaces the chat.
export async function hideChatBehindMembers(page: Page) {
  await page.getByTestId("header-members-button").click();
  await expect(page.locator("aside").filter({ hasText: "Active Members" })).toBeVisible();
  await expect(page.locator("#chat-scroll-container"), "premise: the chat panel is hidden").toBeHidden();
}

/// Back from the members panel to the chat, once the revealed history has laid out.
export async function backToChatFromMembers(page: Page) {
  await page.getByTestId("members-back-to-chat-button").click();
  await expect(page.locator("#chat-scroll-container")).toBeVisible();
  await nextFrames(page);
}

/// Withhold the history's settle events (`scrollend`, and `scroll` for the
/// debounce path on engines without `scrollend`) so a test can put a patch
/// BEFORE the settle deterministically. Nothing is swallowed: `release`
/// re-dispatches one of each kind that was held, after the patch.
///
/// Installed on `window` in the capture phase, which runs before the
/// container's own listeners for these non-bubbling events.
export async function holdSettleEvents(page: Page) {
  await page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const gate = {
      held: { scroll: 0, scrollend: 0 },
      hasScrollend: "onscrollend" in c,
      listener: (e: Event) => {
        if (e.target !== c) return;
        e.stopImmediatePropagation();
        gate.held[e.type as "scroll" | "scrollend"] += 1;
      },
    };
    for (const type of ["scroll", "scrollend"]) {
      window.addEventListener(type, gate.listener, { capture: true });
    }
    (window as any).__riverSettleGate = gate;
  });
}

/// Wait until the reader's own scroll has produced the event that would settle
/// it, still withheld: the premise that the settle comes after the patch.
export async function expectSettleWithheld(page: Page) {
  await expect
    .poll(
      () =>
        page.evaluate(() => {
          const gate = (window as any).__riverSettleGate;
          return gate.hasScrollend ? gate.held.scrollend : gate.held.scroll;
        }),
      { timeout: 5_000, message: "premise: the reader's scroll should have produced a settle event to hold" },
    )
    .toBeGreaterThan(0);
}

export async function releaseSettleEvents(page: Page) {
  await page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const gate = (window as any).__riverSettleGate;
    if (!gate) return;
    for (const type of ["scroll", "scrollend"]) {
      window.removeEventListener(type, gate.listener, { capture: true });
    }
    delete (window as any).__riverSettleGate;
    if (gate.held.scroll > 0) c.dispatchEvent(new Event("scroll"));
    if (gate.held.scrollend > 0) c.dispatchEvent(new Event("scrollend"));
  });
}

const TEST_IMAGE_SVG =
  '<svg xmlns="http://www.w3.org/2000/svg" width="400" height="300"><rect width="400" height="300" fill="#888"/></svg>';

/// Hold every request for `/test-image.svg` (a 400x300 SVG) until `release`.
/// Each mount of the image may ask again, so all of them are held and all
/// released, and any request after `release` is served at once.
export async function holdTestImage(page: Page) {
  const held: Route[] = [];
  let released = false;
  const serve = (route: Route) =>
    route.fulfill({ contentType: "image/svg+xml", body: TEST_IMAGE_SVG }).catch(() => {
      // A request the browser gave up on when its row unmounted.
    });
  await page.route("**/test-image.svg", (route) => (released ? serve(route) : void held.push(route)));
  return {
    requested: () => held.length,
    release: async () => {
      released = true;
      await Promise.all(held.splice(0).map(serve));
    },
  };
}
