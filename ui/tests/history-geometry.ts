import { expect, Page, test } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom } from "./example-room";

// Geometry and event-order helpers shared by the conversation scroll specs.
// Rows are identified by `data-item-key` (a group's first message id), never
// by Dioxus render-order ids.

/// Rendered history rows: display items plus date separators.
export const HISTORY_ROWS = '[data-testid="conversation-history"] > *';

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
export const BOTTOM_THRESHOLD_PX = 100;
/// Slack for fractional layout after a scroll that did land at the bottom.
export const AT_BOTTOM_EPSILON_PX = 4;
/// How far a row the reader is looking at may move and still count as "kept
/// in place". Independent of the app's own 2px slack and 100px band (#732).
export const READING_ROW_BUDGET_PX = 4;

/// The main SHA the known failures below were reproduced on, before the scroll simplification.
const KNOWN_FAILURE_SHA = "739fd683";

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

/// scrollHeight - scrollTop - clientHeight: how far the end of the history is
/// below the visible area. 0 means the newest message is fully in view.
export function distanceFromBottom(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    if (!el) return Number.NaN;
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  });
}

/// Total height of the rendered history, independent of where it is scrolled.
export function historyHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.scrollHeight : Number.NaN;
  });
}

/// Height of the WINDOW onto the history. Shrinks when the composer grows.
export function viewportHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.clientHeight : Number.NaN;
  });
}

export function scrollTop(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.scrollTop : Number.NaN;
  });
}

/// How many newer items the rendered range is holding back, from the
/// history's `data-newer-withheld`.
export function withheld(page: Page): Promise<number> {
  return page
    .getByTestId("conversation-history")
    .getAttribute("data-newer-withheld")
    .then((n) => Number(n));
}

/// Premise shared by the parked-reader tests: the view is outside the
/// bottom band, so an arrival must not be followed.
export async function expectParkedAwayFromEnd(page: Page, why = "premise: parked away from the end") {
  expect(await distanceFromBottom(page), why).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
}

export async function expectSettledAtBottom(page: Page, why: string, timeout = 5_000) {
  await expect
    .poll(() => distanceFromBottom(page), { timeout, message: why })
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
  // Asserted since a hidden panel would make every geometry read return 0.
  await expect(page.locator("#chat-scroll-container")).toBeVisible({ timeout: 5_000 });
  await expectSettledAtBottom(page, "opening a room should land on its newest message");
}

/// Simulate the reader dragging the history with a pointing device.
///
/// The synthetic `wheel` signals reader intent; the `scrollTop` assignment is
/// what moves the viewport, on every engine (`page.mouse.wheel` is unsupported
/// on mobile WebKit). Returns without waiting for the settle, so callers can
/// act before it lands.
export async function readerScrollsTo(page: Page, top: number) {
  await page.evaluate((t) => {
    const el = document.getElementById("chat-scroll-container")!;
    el.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -1 }));
    el.scrollTop = t;
  }, top);
}

/// The same, with NO gesture event at all, and waiting for the settle, so the
/// app has seen the move end before the caller acts. Tests that need an
/// un-settled move assign `scrollTop` themselves.
///
/// Not a contrivance: a native scrollbar drag dispatches no pointer event to
/// the content on Firefox, and find-in-page, focus-driven scrolling and browser
/// scroll restoration produce none either.
export async function readerScrollsWithoutGesture(page: Page, top: number) {
  await page.evaluate(async (t) => {
    const el = document.getElementById("chat-scroll-container")!;
    const settled = new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("the scroll never settled")), 5_000);
      el.addEventListener(
        "scrollend",
        () => {
          clearTimeout(timer);
          resolve();
        },
        { once: true },
      );
    });
    el.scrollTop = t;
    await settled;
  }, top);
}

/// Hold for a moment and assert the view did not move.
///
/// Compares `scrollTop` rather than distance-from-bottom: distance also moves
/// when content grows, so it would tolerate a partial yank of up to the new
/// message's height.
export async function expectStaysPut(page: Page, why: string) {
  const before = await scrollTop(page);
  await page.waitForTimeout(600);
  expect(await scrollTop(page), why).toBeCloseTo(before, 0);
}

/// Add enough history to have somewhere to scroll back through.
export async function fillHistory(page: Page, label = "filler") {
  for (let i = 0; i < 8; i++) {
    await deliver(page, `${label} ${i}: ${"y".repeat(200)}`);
  }
  await expectSettledAtBottom(page, "filler messages should have been followed");
}

/// Two animation frames, so a patch's effects and the layout they cause land.
export function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () => new Promise<void>((r) => requestAnimationFrame(() => requestAnimationFrame(() => r()))),
  );
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

/// A row's top relative to the container's top, or null when it is not in the
/// DOM (deleted from the data, or evicted from the rendered range).
export function rowTop(page: Page, key: string): Promise<number | null> {
  return page.evaluate((k) => {
    const c = document.getElementById("chat-scroll-container")!;
    const row = c.querySelector(`[data-item-key="${CSS.escape(k)}"]`);
    return row ? row.getBoundingClientRect().top - c.getBoundingClientRect().top : null;
  }, key);
}

/// Measure `key` after the patch has settled and again a moment later, so a
/// late correction (or a late yank) counts against the row too.
export async function expectRowHeld(page: Page, key: string, expectedTop: number, why: string) {
  for (let i = 0; i < 2; i++) {
    await nextFrames(page);
    await page.waitForTimeout(300);
    const top = await rowTop(page, key);
    expect(top, `${why} (the row left the DOM)`).not.toBeNull();
    expect(Math.abs(top! - expectedTop), `${why} (moved ${top! - expectedTop}px)`).toBeLessThanOrEqual(
      READING_ROW_BUDGET_PX,
    );
  }
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
