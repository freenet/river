import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { selectListedRoom } from "./example-room";
import {
  newestMessageDrift,
  newestVisibleRow,
  registerHistoryGeometry,
  savedRowDrift,
  type RowPosition,
} from "./history-scroll-geometry";
import {
  AT_BOTTOM_EPSILON_PX,
  DEEP_ROOM_PATH,
  HISTORY_ROWS,
  IN_PLACE_TOLERANCE_PX,
  PARKED_ABOVE_END_PX,
  TALL,
  afterLayoutSettles,
  chatHidden,
  deliver,
  distanceFromBottom,
  endMinus,
  expectDriftWithin,
  expectSettledAtBottom,
  fillHistory,
  hideChat,
  historyHeight,
  openRoomAtBottom,
  readerScrollsToEnd,
  readerScrollsWithoutGesture,
  renderedRowCount,
  revealChat,
  scrollTop,
  viewportHeight,
} from "./history-scroll-helpers";

// Where the history's view goes (ui/src/components/conversation/history_scroll.rs):
// the reader's position is a MESSAGE, not an offset, and only the reader moves it.
//
// The newest visible row is saved with its gap from the container's bottom edge,
// and put back at that gap after anything else changes the history. This suite
// holds the code to that rule everywhere, at the very end of the history too:
//
//   * arrivals (one, a burst, a batched update, a tall one) never pull the view
//     down, whether the reader is exactly at the end, resting a few pixels
//     above it, or deep in history. There is no follow band;
//   * layout changes (a resize there and back, the composer growing and
//     collapsing, a late image, an edit, rows inserted, removed or trimmed
//     above) keep the saved row at its gap, within the geometry the browser
//     allows. Deleting that one row sends the view to the latest message,
//     even when the row above it is still there, and the landing is then
//     preserved like any other position;
//   * a browser clamp is not the reader, so it does not replace the saved row;
//     a reader's scroll whose event has not arrived yet is still the reader's
//     (#723);
//   * the windowed history (#501, #505) keeps the reader's row through
//     arrivals, at-cap drains and the bounded-window trim, and backfill paging
//     keeps working;
//   * hiding the chat (the mobile panels, or a breakpoint) and showing it again
//     brings back the saved row, not the end that arrivals moved on to;
//   * a room opened for the first time this session is placed at its newest
//     message once, and from then on preserved like any other position.
//
// The "Scroll to latest messages" button is covered by
// conversation-native-scroll.spec.ts, room revisits and own sends by their own
// spec; the room switches here only check that a first visit still opens at the
// newest message.
//
// Assumes the example-data build, which exposes `window.__riverTest` for
// delivering INBOUND messages.

/// The scroll model's allowance for a layout move (LAYOUT_SHIFT_ALLOWANCE_PX in
/// ui/src/components/conversation/history_scroll.rs). Only a fixture premise:
/// the tests that need a clamp or a move on one side of it assert that side, so
/// a policy change fails them at setup instead of leaving them vacuous.
const LAYOUT_SHIFT_ALLOWANCE_PX = 200;

/// A draft long enough to take more than PARKED_ABOVE_END_PX off the history.
const LONG_DRAFT = Array.from({ length: 12 }, (_, i) => `draft line ${i}`).join("\n");

/// The in-page helpers the evaluations below share (`window.__riverScroll`).
/// Message rows are found from `[id^="msg-"]`, not from the implementation's own
/// row attribute, so these tests stay a contract on behaviour.
type ScrollHelpers = {
  /// The newest message row with any part inside `c`, and how far its top sits
  /// above `c`'s bottom edge.
  newestVisible(c: HTMLElement): RowPosition | null;
  /// The history's message row whose text includes `text`.
  rowWithText(text: string): HTMLElement | null;
  /// Call `then` once the row with `text` is in the history; returns a stop.
  patchLanded(text: string, then: (row: HTMLElement) => void): () => void;
};

declare global {
  interface Window {
    __riverScroll: ScrollHelpers;
    // Per-test records, each set by the test that reads it and absent until then.
    /// The type of the container's latest `scroll`/`scrollend` event.
    __riverLastScrollEvent?: string | null;
    __riverCollapse?: CollapseUnderArrival;
    __riverSameFrame?: { collapsed: boolean; grew: number };
  }
}

/// Runs in the page (`addInitScript`), so it must be self-contained.
function installScrollHelpers() {
  const HISTORY = '[data-testid="conversation-history"]';
  const helpers: ScrollHelpers = {
    newestVisible(c) {
      const geo = window.__riverHistoryGeometry;
      if (!geo) throw new Error("history geometry is not installed");
      return geo.newestVisible(c);
    },
    rowWithText(text) {
      const rows = document.querySelectorAll<HTMLElement>(`${HISTORY} [id^="msg-"]`);
      return Array.from(rows).find((row) => row.textContent?.includes(text)) ?? null;
    },
    patchLanded(text, then) {
      const observer = new MutationObserver(() => {
        const row = helpers.rowWithText(text);
        if (!row) return;
        observer.disconnect();
        then(row);
      });
      observer.observe(document.querySelector(HISTORY)!, { childList: true, subtree: true });
      return () => observer.disconnect();
    },
  };
  window.__riverScroll = helpers;
}

// Before every navigation of every test, so each evaluation can rely on it.
test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
  await page.addInitScript(installScrollHelpers);
});

/// Simulate the reader dragging the history with a pointing device. Returns
/// where it landed, read in the same task, before any scroll event or restore
/// can answer it.
///
/// A synthetic `wheel` followed by a `scrollTop` assignment rather than
/// `page.mouse.wheel`, which is unsupported on mobile WebKit. Nothing listens for
/// gestures (the `scrollTop` write is the reader's scroll); the `wheel` keeps
/// the helper realistic. `wheel: false` leaves it out: after one, WebKit
/// dispatches every later `scroll` event on the element twice (measured on a
/// bare scroller outside the app, 2026-10-01; Chromium and Firefox send one).
function readerScrollsTo(page: Page, top: number, { wheel = true } = {}): Promise<number> {
  return page.evaluate(
    ({ t, wheel }) => {
      const el = document.getElementById("chat-scroll-container")!;
      if (wheel) el.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -1 }));
      el.scrollTop = t;
      return el.scrollTop;
    },
    { t: top, wheel },
  );
}

/// `readerScrollsTo`, then wait until the view is where it was aimed.
async function readerParksAt(page: Page, target: number) {
  await readerScrollsTo(page, target);
  await expect
    .poll(() => offsetDrift(page, target), {
      timeout: 5_000,
      message: "premise: the reader's scroll should land where it was aimed",
    })
    .toBeLessThanOrEqual(1);
}

/// Hold for a moment and assert the view did not move.
///
/// Compares `scrollTop` rather than distance-from-bottom: distance also moves
/// when content grows, so it would tolerate a partial yank of up to the new
/// message's height.
async function expectStaysPut(page: Page, why: string) {
  const before = await scrollTop(page);
  await page.waitForTimeout(600);
  expect(await scrollTop(page), why).toBeCloseTo(before, 0);
}

/// The newest history message with any part inside the scroll container, and
/// how far its top sits above the container's bottom edge.
function newestVisibleMessage(page: Page): Promise<RowPosition | null> {
  return newestVisibleRow(page);
}

/// The newest visible message, required to exist.
async function savedRow(page: Page): Promise<RowPosition> {
  const row = await newestVisibleMessage(page);
  expect(row, "premise: a message should be visible").not.toBeNull();
  return row!;
}

/// How far a remembered position has drifted. Infinity when the row is gone, or
/// (with `newest`) when another message is now the newest visible one: a
/// missing row is a failure, never a zero drift.
function positionDrift(page: Page, before: RowPosition, newest: boolean): Promise<number> {
  return newest ? newestMessageDrift(page, before) : savedRowDrift(page, before);
}

/// `before`'s row is back at its gap. `newest` (the default) also requires it to
/// be the newest visible message again; pass false when a later row may show
/// inside the view without the saved row having moved.
///
/// `hold` asks for bounded evidence that nothing undoes it a moment later, for
/// scenarios a later callback could reverse: after converging, five samples over
/// 500ms, any one out of budget failing with the whole sequence. Bounded
/// evidence, not proof of indefinite stability.
async function expectInPlace(
  page: Page,
  before: RowPosition,
  why: string,
  { newest = true, hold = false, tolerance = IN_PLACE_TOLERANCE_PX } = {},
) {
  await expectDriftWithin(page, () => positionDrift(page, before, newest), why, { hold, tolerance });
}

/// The message `newestVisibleMessage` returned is the newest visible one again,
/// at the same gap. Keeps the 2px bound these older tests were written against.
async function expectSameMessageInPlace(page: Page, before: RowPosition, why: string) {
  await expectInPlace(page, before, why, { tolerance: 2 });
}

/// What the arrivals at the end must leave: the reader's row at its gap (held),
/// and the end of the history below the view rather than in it.
async function expectNotFollowed(page: Page, before: RowPosition, why: string) {
  await expectInPlace(page, before, why, { newest: false, hold: true });
  expect(
    await distanceFromBottom(page),
    `${why}: the view was pulled down to the new end`,
  ).toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
}

/// Fill Team Chat Room, have the reader scroll to its very end themselves, and
/// remember the newest message they can see there.
async function parkAtEnd(page: Page): Promise<RowPosition> {
  await openRoomAtBottom(page, "Team Chat Room");
  await fillHistory(page);
  await afterLayoutSettles(page);
  return savedRow(page);
}

/// Park at `top` and remember the newest message in that view. `0` is the
/// oldest rendered history. `"half"` is the middle, measured after the room
/// is filled — the height does not exist before then.
async function parkAt(page: Page, top: number | "half"): Promise<RowPosition> {
  await openRoomAtBottom(page, "Team Chat Room");
  await fillHistory(page);
  const destination = top === "half" ? Math.floor((await historyHeight(page)) / 2) : top;
  await readerScrollsTo(page, destination);
  await expect
    .poll(() => distanceFromBottom(page), {
      timeout: 5_000,
      message: "premise: the reader should be parked above the bottom",
    })
    .toBeGreaterThan(PARKED_ABOVE_END_PX);
  // The scroll has to have landed before we look at what is on screen.
  await afterLayoutSettles(page);
  return savedRow(page);
}

/// The ways content can arrive at the end of the history.
const ARRIVALS = [
  {
    kind: "one arrival",
    run: (page: Page) => deliver(page, `one arrival at the end: ${"q".repeat(200)}`),
  },
  {
    kind: "a burst of separate arrivals",
    run: async (page: Page) => {
      for (let i = 1; i <= 6; i++) await deliver(page, `burst arrival ${i}`);
    },
  },
  {
    kind: "a batched update",
    run: async (page: Page) => {
      await callRiverTest(page, "appendMessages", 5);
      await expect(page.getByText("batched arrival 04")).toBeAttached({ timeout: 5_000 });
    },
  },
  {
    kind: "a tall arrival",
    run: async (page: Page) => {
      await callRiverTest(page, "appendMessage", TALL("tall arrival at the end"));
      await expect(page.getByText("tall arrival at the end")).toBeAttached({ timeout: 5_000 });
    },
  },
] as const;

// The rule at its sharpest: a reader exactly at the end of the history is not
// followed either. What they could see stays where it was, and the arrival
// waits below the view (decisions 1 and 2 of HISTORY-SCROLL-SIMPLIFICATION-PLAN.md).
test.describe("Arrivals at the end of the history do not move the view", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("the first open's placement at the newest message is kept through the next arrival", async ({ page }) => {
    // No reader scroll at all: only the room's initial placement put the view
    // here, and it is a starting position, not a mode.
    await openRoomAtBottom(page, "Team Chat Room");
    await afterLayoutSettles(page);
    const placed = await savedRow(page);
    await deliver(page, `arrival after the first open: ${"o".repeat(200)}`);
    await expectNotFollowed(page, placed, "an arrival after the room's first placement moved the reader's row");
  });

  for (const arrival of ARRIVALS) {
    test(`at the exact end, ${arrival.kind} keeps the newest row's gap`, async ({ page }) => {
      const before = await parkAtEnd(page);
      expect(await distanceFromBottom(page), "premise: the reader is at the very end").toBeLessThanOrEqual(
        AT_BOTTOM_EPSILON_PX,
      );
      const heightBefore = await historyHeight(page);
      await arrival.run(page);
      expect(
        (await historyHeight(page)) - heightBefore,
        "premise: the arrival grows the history by more than the tolerance could hide",
      ).toBeGreaterThan(4 * IN_PLACE_TOLERANCE_PX);
      await expectNotFollowed(page, before, `${arrival.kind} at the end moved the reader's row`);
    });
  }

  // Inside what used to be the 100px follow band. The band now only hides the
  // scroll-to-latest button; a reader resting there is preserved like anyone.
  test("a reader resting a little above the end keeps their row through an arrival", async ({ page }) => {
    const ABOVE_END_PX = 40;
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    const target = await endMinus(page, ABOVE_END_PX);
    // The view is checked at the moment the scroll came to rest, before any
    // later callback could move it.
    const settledAt = await readerScrollsWithoutGesture(page, target);
    expect(Math.abs(settledAt - target), "the reader's scroll was moved as it came to rest").toBeLessThanOrEqual(1);
    await afterLayoutSettles(page);
    const distance = await distanceFromBottom(page);
    expect(distance, "premise: the reader rests inside the old follow band").toBeLessThan(PARKED_ABOVE_END_PX);
    expect(distance, "premise: and visibly above the end").toBeGreaterThan(ABOVE_END_PX / 2);
    const before = await savedRow(page);

    await deliver(page, `arrival near the end: ${"n".repeat(200)}`);
    await expectNotFollowed(page, before, "a reader resting a little above the end was moved by an arrival");
  });

  test("at the end, a draft growing the composer, arrivals under it, and clearing it keep the newest row's gap", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await afterLayoutSettles(page);
    const before = await savedRow(page);
    const roomyViewport = await viewportHeight(page);

    // The gap is measured from the container's BOTTOM edge, which the composer
    // takes height off: the reader's row moves up with the growing composer
    // rather than being covered by it.
    await page.getByTestId("message-input").fill(LONG_DRAFT);
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message: "premise: the composer should take a material height off the history",
      })
      .toBeLessThan(roomyViewport - PARKED_ABOVE_END_PX);
    await expectInPlace(page, before, "the composer grew over the reader's row", { hold: true });

    // Each one is checked, so a view that holds for one arrival and then
    // ratchets toward the end is caught.
    for (let i = 1; i <= 6; i++) {
      await deliver(page, `arrival ${i}`);
      await expectInPlace(page, before, `message ${i} arrived under an open draft and moved the reader's row`, {
        newest: false,
      });
    }

    // Clearing the draft gives the height back, so the container GROWS and the
    // browser may clamp `scrollTop` on its own. The clamp is layout's.
    await page.getByTestId("message-input").fill("");
    await expect
      .poll(() => viewportHeight(page), { timeout: 5_000 })
      .toBeGreaterThan(roomyViewport - IN_PLACE_TOLERANCE_PX);
    await expectInPlace(page, before, "the draft was cleared and the reader's row did not keep its gap", {
      newest: false,
      hold: true,
    });
    await deliver(page, "arrived after the draft was cleared");
    await expectNotFollowed(page, before, "the arrival after the draft was cleared moved the reader's row");
  });

  test("a mid-list insert above the newest row, which does not remount it, keeps that row's gap", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await afterLayoutSettles(page);
    const before = await savedRow(page);

    // Tag the last row so we can prove afterwards that it was diffed, not
    // remounted: only the content-change restore can have put it back then.
    //
    // It holds because of how the fixture ends, not by luck: "Team Chat Room"
    // finishes with messages from different authors, so its last group is a
    // single message whose key (its first message's id) cannot change when
    // something is inserted before it. `insertMessageBeforeLast` also signs
    // with its own key, so it can never merge into that group.
    await page.locator(HISTORY_ROWS).last().evaluate((row) => {
      (row as any).__riverProbe = "last-row";
    });

    const heightBefore = await historyHeight(page);
    await callRiverTest(page, "insertMessageBeforeLast", "inserted above the last row: " + "x".repeat(400));
    await expect
      .poll(() => historyHeight(page), {
        timeout: 5_000,
        message: "premise: the insert should grow the history materially",
      })
      .toBeGreaterThan(heightBefore + PARKED_ABOVE_END_PX);

    await expectInPlace(page, before, "content grew above the newest row and the row did not keep its gap", {
      hold: true,
    });

    const lastRowSurvived = await page
      .locator(HISTORY_ROWS)
      .last()
      .evaluate((row) => (row as any).__riverProbe === "last-row");
    expect(
      lastRowSurvived,
      "the last row remounted, so this exercised a remount rather than the content-change restore it is meant to pin",
    ).toBe(true);
  });

  test("a reader who scrolls back to the end themselves is preserved there through the next arrival", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    await readerScrollsTo(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);
    await deliver(page, "arrived while reading history");
    await expectStaysPut(page, "a message arrived while the reader was scrolled up and moved the view");

    // Coming back to the end is a position like any other: it is captured, and
    // the next arrival lands below it.
    await readerScrollsWithoutGesture(page, await historyHeight(page));
    await expectSettledAtBottom(page, "the reader's own scroll should reach the bottom");
    await afterLayoutSettles(page);
    const atEnd = await savedRow(page);
    await deliver(page, "arrived after the reader scrolled back down");
    await expectNotFollowed(page, atEnd, "an arrival after the reader came back to the end moved their row");
  });

  test("a reader scroll that produces no gesture event is kept through an arrival", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    // No `wheel`, no `pointerdown`, no `touchstart`: only the scroll event itself
    // can tell the reader moved. This is the shape a native scrollbar drag on
    // Firefox, find-in-page or focus-driven scrolling takes.
    await readerScrollsWithoutGesture(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);
    await afterLayoutSettles(page);
    const before = await savedRow(page);

    await deliver(page, "arrived after a gesture-less scroll");
    await expectInPlace(page, before, "the reader scrolled up without a gesture event and an arrival moved them", {
      hold: true,
    });
  });
});

/// `scrollTop`, `clientHeight` and `scrollHeight` of the history's container.
type Geometry = { top: number; client: number; height: number };

/// What `collapseUnderArrival` saw: the reader's row and the geometry before
/// the composer was cleared, right after its collapse, and when the arrival's
/// row landed, and the order the arrival's patch and the clamp's scroll event
/// came in.
type CollapseUnderArrival = {
  row: RowPosition | null;
  before: Geometry;
  collapsed: Geometry | null;
  atPatch: Geometry | null;
  order: string[];
};

/// How many times `collapseUnderArrival` may set up the collapse to get the
/// order it wants (see there).
const COLLAPSE_ATTEMPTS = 3;

/// Open Team Chat Room, fill the composer to its cap, then clear it and deliver a
/// 30-line inbound message from the input event, after the app's own handler has
/// collapsed the composer. `fill` clears it with Playwright's `fill`;
/// `after-a-frame` in a task queued from a frame callback, so the clamp's
/// scroll event and resize wait for the next frame, a frame interval away.
///
/// Which comes first, the arrival's patch or the clamp's scroll event, is up to
/// the engine's scheduling: a loaded host can run a frame between any two tasks.
/// An attempt that does not produce `want` must still keep the reader's row (it
/// is the other order, which the clamp-to-the-end rule covers); the reader then
/// scrolls back to the end, and the setup runs again, up to `COLLAPSE_ATTEMPTS`
/// times. The caller asserts the order of the last. `either` takes the first
/// attempt, in whichever order it came.
async function collapseUnderArrival(
  page: Page,
  clear: "fill" | "after-a-frame",
  want: CollapseOrder,
): Promise<CollapseUnderArrival> {
  await openRoomAtBottom(page, "Team Chat Room");
  const roomyViewport = await viewportHeight(page);
  await page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    for (const type of ["scroll", "scrollend"]) {
      c.addEventListener(type, () => {
        window.__riverLastScrollEvent = type;
      });
    }
  });
  const seen: string[] = [];
  let rec: CollapseUnderArrival | null = null;
  for (let attempt = 1; attempt <= COLLAPSE_ATTEMPTS; attempt++) {
    rec = await collapseOnce(page, clear, `collapse arrival ${attempt}`, roomyViewport);
    seen.push(rec.order.join(" → "));
    if (want === "either" || rec.order[0] === want) break;
    await expectInPlace(page, rec.row!, "the other order of the collapse and the arrival moved the reader's row", {
      newest: false,
    });
    // The next collapse has to clamp at the end again.
    await readerScrollsToEnd(page);
    await afterLayoutSettles(page);
  }
  test.info().annotations.push({ type: "collapse order", description: seen.join(" | ") });
  return rec!;
}

/// One attempt of `collapseUnderArrival`, with the arrival marked `marker`.
async function collapseOnce(
  page: Page,
  clear: "fill" | "after-a-frame",
  marker: string,
  roomyViewport: number,
): Promise<CollapseUnderArrival> {
  const input = page.getByTestId("message-input");
  await page.evaluate(() => (window.__riverLastScrollEvent = null));
  const capDraft = Array.from({ length: 30 }, (_, i) => `draft line ${i}`).join("\n");
  await input.fill(capDraft);
  await expect
    .poll(() => viewportHeight(page), {
      timeout: 5_000,
      message: "premise: the composer should take more than the layout allowance off the history",
    })
    .toBeLessThan(roomyViewport - LAYOUT_SHIFT_ALLOWANCE_PX);
  expect(
    await input.evaluate((el) => el.scrollHeight > el.clientHeight),
    "premise: a 30-line draft should hold the composer at its cap",
  ).toBe(true);
  await expectSettledAtBottom(page, "premise: the reader's row kept its gap, so the view is still at the end");
  // A scroll event still pending from the composer's growth would be taken for
  // the clamp's.
  await page.waitForFunction(
    () => window.__riverLastScrollEvent === "scrollend",
    undefined,
    { timeout: 5_000 },
  );

  await page.evaluate((text) => {
    const c = document.getElementById("chat-scroll-container")!;
    const read = (): Geometry => ({ top: c.scrollTop, client: c.clientHeight, height: c.scrollHeight });
    const rec: CollapseUnderArrival = {
      row: window.__riverScroll.newestVisible(c),
      before: read(),
      collapsed: null,
      atPatch: null,
      order: [],
    };
    window.__riverCollapse = rec;
    // Capturing on `document`, so it is noted before the app's own listener on
    // the container handles the event.
    const onScroll = (e: Event) => {
      if (e.target !== c || !rec.collapsed) return;
      rec.order.push("scroll");
      document.removeEventListener("scroll", onScroll, { capture: true });
    };
    document.addEventListener("scroll", onScroll, { capture: true });
    window.__riverScroll.patchLanded(text.slice(0, 40), () => {
      rec.order.push("patch");
      rec.atPatch = read();
    });
    // On `document`, so after the app's input handler: reading the geometry
    // forces the collapse and its clamp now. The hook defers its state change,
    // as a network update does, so the arrival renders in a later task.
    document.addEventListener(
      "input",
      () => {
        rec.collapsed = read();
        window.__riverTest!.appendMessage(text);
      },
      { once: true },
    );
  }, TALL(marker, 30));
  if (clear === "fill") {
    await input.fill("");
  } else {
    await input.evaluate(
      (el: HTMLTextAreaElement) =>
        new Promise<void>((resolve) =>
          requestAnimationFrame(() =>
            setTimeout(() => {
              el.value = "";
              el.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "deleteContentBackward" }));
              resolve();
            }),
          ),
        ),
    );
  }
  await expect(page.getByText(marker).last()).toBeAttached({ timeout: 5_000 });
  await afterLayoutSettles(page);
  return page.evaluate(() => window.__riverCollapse!);
}

/// Which of the arrival's patch and the clamp's scroll event is wanted first, or
/// `either` where the engine does not fix it.
type CollapseOrder = "patch" | "scroll" | "either";

/// The premises of `collapseUnderArrival`: the collapse clamped the view up past
/// the allowance with its lower edge held by the container's growth, and the
/// arrival grew the history materially. `first` is which of the arrival's patch
/// and the clamp's scroll event is expected first: with the patch first, no
/// frame has run since the clamp, so neither its scroll event nor the
/// ResizeObserver has handled it before the history grew.
function expectCollapseUnderArrival(
  { row, before, collapsed, atPatch, order }: CollapseUnderArrival,
  first: CollapseOrder,
) {
  expect(row, "premise: a message should be visible before the collapse").not.toBeNull();
  expect(collapsed, "premise: the composer should collapse inside the input handler").not.toBeNull();
  expect(
    before.top - collapsed!.top,
    "premise: the collapse should clamp the view up by more than the layout allowance",
  ).toBeGreaterThan(LAYOUT_SHIFT_ALLOWANCE_PX);
  expect(
    Math.abs(collapsed!.top + collapsed!.client - (before.top + before.client)),
    "premise: the container's growth should keep the view's lower edge where it was",
  ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
  const observed = `(observed: ${order.join(" → ")})`;
  if (first === "either") {
    expect(order, `premise: the arrival's patch should be observed ${observed}`).toContain("patch");
  } else {
    expect(
      order[0],
      `premise: which of the arrival's patch and the clamp's scroll event comes first ${observed}`,
    ).toBe(first);
  }
  expect(
    atPatch!.height - collapsed!.height,
    "premise: the arrival should grow the history materially",
  ).toBeGreaterThan(PARKED_ABOVE_END_PX);
}

/// The reader's row kept its gap through the collapse, and through the next
/// tall arrival.
async function expectRowKeptAfterCollapse(page: Page, row: RowPosition) {
  await expectNotFollowed(page, row, "the composer collapsed under a tall arrival and the reader's row moved");
  await callRiverTest(page, "appendMessage", TALL("after the collapse", 30));
  await expect(page.getByText("after the collapse").last()).toBeAttached({ timeout: 5_000 });
  await expectNotFollowed(page, row, "the arrival after the collapse moved the reader's row");
}

// Layout changes with no message state behind them: a resize, a rewrap, the
// composer. The newest row the reader could see keeps its gap from the bottom.
test.describe("Layout changes at the end keep the newest row (#486)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // A reflow that only trips on some font stacks (Linux CI's, not macOS's):
  // Chromium clamps scrollTop partway through the reflow, then the history comes
  // out taller. This makes the same clamp happen on every engine. Read as the
  // reader, the clamp would replace the saved row with wherever it left the view.
  test("a reflow that clamped the view on its way to a taller history keeps the newest row @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await afterLayoutSettles(page);
    const before = await savedRow(page);
    const { clamp, laidOutAgain, belowFinalEnd } = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      const content = document.getElementById("chat-content")!;
      const rows = c.querySelectorAll('[data-testid="conversation-history"] > *');
      const newest = rows[rows.length - 1] as HTMLElement;
      const before = c.scrollTop;
      const heightBefore = c.scrollHeight;
      const widthBefore = content.clientWidth;
      // Short, clamped, then rewrapped taller: all inside one task.
      newest.style.display = "none";
      const clamped = c.scrollTop;
      newest.style.display = "";
      content.style.maxWidth = `${content.clientWidth - 120}px`;
      // Read synchronously, so the layout has been redone before we look.
      const laidOutAgain =
        c.scrollHeight !== heightBefore || content.clientWidth !== widthBefore;
      return {
        clamp: before - clamped,
        laidOutAgain,
        belowFinalEnd: c.scrollHeight - c.clientHeight - c.scrollTop,
      };
    });
    expect(clamp, "premise: hiding the newest row must clamp the view").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX
    );
    // An INTERMEDIATE clamp: it ends short of the final end, so only the
    // allowance for a changed layout can tell it from the reader. A clamp to the
    // final end is the business of "A clamp to the end is not the reader".
    expect(
      clamp,
      "premise: the intermediate clamp is within the layout allowance",
    ).toBeLessThanOrEqual(LAYOUT_SHIFT_ALLOWANCE_PX);
    expect(
      belowFinalEnd,
      "premise: the clamped view ends above the taller history's end",
    ).toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
    expect(
      laidOutAgain,
      "premise: the rewrap must change the history's height or the width it wraps at"
    ).toBe(true);
    await expectInPlace(page, before, "the reflow's own clamp was read as the reader scrolling up", { hold: true });
  });

  test("history that grows in the same frame the composer collapses keeps the newest row", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const roomyViewport = await viewportHeight(page);
    await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      for (const type of ["scroll", "scrollend"]) {
        c.addEventListener(type, () => {
          window.__riverLastScrollEvent = type;
        });
      }
    });
    await page.getByTestId("message-input").fill(LONG_DRAFT);
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message:
          "the composer did not grow enough that clearing it would clamp the " +
          "view, so this test is not exercising the same-frame race",
      })
      .toBeLessThan(roomyViewport - PARKED_ABOVE_END_PX);
    await expectSettledAtBottom(page, "premise: the reader's row kept its gap, so the view is still at the end");
    // A scroll event still pending from the composer's growth would capture
    // after the clamp and hide the race.
    await page.waitForFunction(
      () => window.__riverLastScrollEvent === "scrollend",
      undefined,
      { timeout: 5_000 },
    );
    const before = await savedRow(page);

    // On `document`, so it runs after the app's own input handler has
    // collapsed the composer, before any frame.
    const GROWTH_PX = 80;
    // Same frame as the collapse's clamp: one scroll event, two layout changes.
    await page.evaluate(
      ({ grow, collapsedAbove }) => {
        const c = document.getElementById("chat-scroll-container")!;
        const rows = c.querySelectorAll("[data-item-key]");
        const newest = rows[rows.length - 1] as HTMLElement;
        const onInput = () => {
          // Reading clientHeight forces the collapse (and the scrollTop clamp) now.
          const collapsed = c.clientHeight > collapsedAbove;
          const before = c.scrollHeight;
          newest.style.paddingBottom = `${grow}px`;
          window.__riverSameFrame = {
            collapsed,
            grew: c.scrollHeight - before,
          };
        };
        document.addEventListener("input", onInput, { once: true });
      },
      { grow: GROWTH_PX, collapsedAbove: roomyViewport - PARKED_ABOVE_END_PX },
    );
    await page.getByTestId("message-input").fill("");

    const premise = await page.evaluate(() => window.__riverSameFrame!);
    expect(
      premise.collapsed,
      "premise: the composer should collapse inside the input handler",
    ).toBe(true);
    expect(premise.grew, "premise: the newest row should have grown").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );
    await expectInPlace(
      page,
      before,
      "the history grew in the same frame the composer collapsed and the reader's row moved",
      { hold: true },
    );
    expect(
      await distanceFromBottom(page),
      "the growth below the reader's row pulled the view down to the new end",
    ).toBeGreaterThan(GROWTH_PX / 2);
  });

  // The same race at full size. A cap-height composer collapsing clamps the view
  // up by far more than the layout allowance, and an arrival that renders before
  // the clamp's scroll event leaves the view short of the new end, so neither
  // the allowance nor the clamp-to-the-end rule covers it. What does: the lower
  // edge of the view did not move.
  //
  // Chromium dispatches input at the start of a frame, so Playwright's `fill`
  // has the clamp's scroll event out before the arrival can render. Clearing in
  // a task just after a frame instead leaves the arrival a frame interval to
  // land in, on every engine; the second test keeps the engines' own order.
  test("a cap-height composer collapsing under a tall arrival keeps the newest row", async ({ page }) => {
    const collapse = await collapseUnderArrival(page, "after-a-frame", "patch");
    expectCollapseUnderArrival(collapse, "patch");
    await expectRowKeptAfterCollapse(page, collapse.row!);
  });

  test("a cap-height composer collapsing under a tall arrival keeps the newest row, in the engine's input order", async ({
    page,
    browserName,
  }) => {
    // WebKit renders the arrival before the clamp's scroll event (the review's
    // reproduction). Chromium's input is aligned to the frame, so its own order
    // is the other one. Firefox's depends on load: one 20-run sample at a load
    // average of ~40-70 saw the scroll event first in three attempts in a row,
    // so its order is recorded (the "collapse order" annotation), not asserted;
    // the test above establishes the arrival first on Firefox too.
    const first: CollapseOrder =
      browserName === "webkit" ? "patch" : browserName === "chromium" ? "scroll" : "either";
    const collapse = await collapseUnderArrival(page, "fill", first);
    expectCollapseUnderArrival(collapse, first);
    await expectRowKeptAfterCollapse(page, collapse.row!);
  });
});

// The reader's position is a message, not an offset: whichever message is the
// newest one on screen stays where it was when the window changes shape.
test.describe("The newest visible message stays in place", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  for (const start of ["end-of-history", "top-of-history", "mid-history"] as const) {
    test(`the same message is still in place after resizing there and back from the ${start} @fractional-geometry`, async ({
      page,
    }) => {
      const before =
        start === "end-of-history"
          ? await parkAtEnd(page)
          : start === "mid-history"
            ? await parkAt(page, "half")
            : await parkAt(page, 0);
      const heightBefore = await historyHeight(page);
      await page.setViewportSize({ width: 380, height: 900 });
      // How much a reflow grows the history depends on where the text wraps:
      // measured on this fixture, 1280 -> 700 grows it by *zero* pixels, and
      // 1280 -> 380 by several hundred. If a fixture or layout change flattens
      // that, this fails loudly instead of going quietly vacuous.
      await expect
        .poll(() => historyHeight(page), {
          timeout: 5_000,
          message: "premise: narrowing the window should make the history taller",
        })
        .toBeGreaterThan(heightBefore + PARKED_ABOVE_END_PX);
      await expectSameMessageInPlace(page, before, "the resize moved the message the reader was looking at");
      if (start !== "end-of-history") {
        expect(
          await distanceFromBottom(page),
          "narrowing moved the parked reader to the end",
        ).toBeGreaterThan(PARKED_ABOVE_END_PX);
      }
      await page.setViewportSize({ width: 1280, height: 900 });
      await expectSameMessageInPlace(
        page,
        before,
        "resizing back did not return the reader's message to where it was",
      );
      if (start !== "end-of-history") {
        expect(
          await distanceFromBottom(page),
          "widening moved the parked reader to the end",
        ).toBeGreaterThan(PARKED_ABOVE_END_PX);
      }
    });
  }

  // The reader's position is measured from the container's BOTTOM edge, and the
  // composer takes height off that edge. So a growing draft moves a parked
  // reader's text up with it rather than covering it, and clearing the draft
  // moves it back. Intentional: the newest line they could see stays in sight.
  test("a parked reader's message keeps its gap from the bottom as the composer grows and clears @fractional-geometry", async ({
    page,
  }) => {
    const before = await parkAt(page, "half");
    const roomy = await viewportHeight(page);

    await page.getByTestId("message-input").fill(LONG_DRAFT);
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message: "premise: the draft should take a material height off the history",
      })
      .toBeLessThan(roomy - PARKED_ABOVE_END_PX);
    await expectInPlace(
      page,
      before,
      "the composer grew and the reader's message did not keep its gap from the bottom edge",
      { hold: true },
    );

    await page.getByTestId("message-input").fill("");
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message: "premise: clearing the draft should give the history its height back",
      })
      .toBeGreaterThan(roomy - IN_PLACE_TOLERANCE_PX);
    await expectInPlace(
      page,
      before,
      "the draft was cleared and the reader's message did not return to its gap",
      { hold: true },
    );
  });

  // After a settled resize the restore's correction must have been recorded:
  // against a stale record, a small reader move reads as the layout's own move
  // and is put back. So the move is SMALL on purpose, inside the layout
  // allowance, and the resize is chosen so the reader's offset stays within that
  // allowance of the pre-resize one.
  test("a small reader scroll after a settled resize is kept @fractional-geometry", async ({ page }) => {
    // 1280 -> 1230 rewraps the filler rows a little: measured on this fixture,
    // the reachable end moves by 20-60px across the five engine projects, where
    // 1200 moves it up to the allowance and 1240 sometimes not at all.
    const RESIZED_WIDTH = 1230;
    const SMALL_MOVE_PX = 150;
    const atEnd = await parkAtEnd(page);
    const layout = () =>
      page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        const content = document.getElementById("chat-content")!;
        return {
          top: c.scrollTop,
          viewport: c.clientHeight,
          wrapWidth: content.clientWidth,
        };
      });
    const before = await layout();
    await page.setViewportSize({ width: RESIZED_WIDTH, height: 900 });
    await expectInPlace(page, atEnd, "the resize did not keep the reader's row at its gap", { hold: true });
    const after = await layout();
    expect(after.wrapWidth, "premise: the resize changes the width the history wraps at").not.toBe(
      before.wrapWidth,
    );
    expect(Math.min(before.viewport, after.viewport), "premise: the history is laid out").toBeGreaterThan(0);
    const moved = after.top - before.top;
    expect(
      Math.abs(moved),
      `premise: the resize's own correction is a small move (scrollTop moved ${moved}px)`,
    ).toBeLessThanOrEqual(LAYOUT_SHIFT_ALLOWANCE_PX);
    expect(
      Math.abs(moved - SMALL_MOVE_PX),
      `premise: the reader's offset stays within the allowance of the pre-resize one (scrollTop moved ${moved}px)`,
    ).toBeLessThanOrEqual(LAYOUT_SHIFT_ALLOWANCE_PX);

    const target = after.top - SMALL_MOVE_PX;
    expect(target, "premise: the reader's target is inside the history").toBeGreaterThan(0);
    const landed = await readerScrollsTo(page, target);
    expect(Math.abs(landed - target), "premise: the reader's scroll lands where it was aimed").toBeLessThanOrEqual(1);
    await expectDriftWithin(page, () => offsetDrift(page, target), "the reader's small scroll after a resize was put back", {
      hold: true,
    });
    const parked = await savedRow(page);

    await deliver(page, "arrival after the reader left");
    await expectInPlace(page, parked, "an arrival moved a reader who scrolled up a little after a resize", {
      hold: true,
    });
  });

  test("a reader scroll in the same frame as a width change is kept", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    // One task: the scroll event and the rewrap's layout change land together.
    const laidOut = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      const content = document.getElementById("chat-content")!;
      const heightBefore = c.scrollHeight;
      c.scrollTop = 0;
      content.style.maxWidth = `${content.clientWidth - 120}px`;
      // Read synchronously, so the layout has been redone before we look.
      return { heightBefore, heightAfter: c.scrollHeight };
    });
    expect(
      laidOut.heightAfter,
      "premise: the width change must change the history's scrollHeight, or this test cannot tell a layout change from a reader's scroll"
    ).not.toBe(laidOut.heightBefore);
    // A poll would pass on its first sample, before the scroll event and the
    // ResizeObserver have run.
    await afterLayoutSettles(page);
    expect(
      await distanceFromBottom(page),
      "the reader's scroll was lost to the width change"
    ).toBeGreaterThan(PARKED_ABOVE_END_PX);
    const parked = await savedRow(page);

    await deliver(page, "arrival after a same-frame scroll");
    await expectInPlace(page, parked, "an arrival moved a reader whose scroll shared a frame with a rewrap", {
      hold: true,
    });
  });

  // An edit is a content change to a row the reader is not anchored on: the
  // edited message, and its edit form before it, grow above them, and their row
  // stays where it was.
  //
  // The message is parked well inside the view, so that the form it opens stays
  // on screen once the reader's row has been put back below it: a focused
  // textarea moved out of view is scrolled back by WebKit, which is the browser
  // revealing the focus (a reader's scroll, by the module's rules), not a
  // restore. Where the example room puts its own messages varies from run to
  // run (its keys are generated), so the message is chosen by position. (No
  // `fillHistory`: the edit's state change drops the test hook's fillers, whose
  // authors are not members.)
  test("an edit that grows a message above the reader keeps their row", async ({ page }) => {
    const OWN_ROW_TOP_PX = 350;
    await openRoomAtBottom(page, "Your Private Room");
    const pick = await page.evaluate((want) => {
      const c = document.getElementById("chat-scroll-container")!;
      const top = c.getBoundingClientRect().top;
      const max = c.scrollHeight - c.clientHeight;
      for (const row of document.querySelectorAll<HTMLElement>('#chat-content [id^="msg-"]:has(.bg-accent)')) {
        const target = Math.round(c.scrollTop + row.getBoundingClientRect().top - top - want);
        if (target >= 0 && target <= max) return { id: row.id, target };
      }
      return null;
    }, OWN_ROW_TOP_PX);
    expect(pick, `premise: an own message can sit ${OWN_ROW_TOP_PX}px down the view`).not.toBeNull();
    const ownId = pick!.id;
    const ownHeight = () => page.evaluate((id) => document.getElementById(id)!.getBoundingClientRect().height, ownId);
    const heightBefore = await ownHeight();
    await readerParksAt(page, pick!.target);
    await afterLayoutSettles(page);
    expect(
      await distanceFromBottom(page),
      "premise: the history is taller than the view, so the reader is up in it",
    ).toBeGreaterThan(PARKED_ABOVE_END_PX);
    const before = await savedRow(page);
    expect(before.id, `premise: the reader's newest visible row is not the one being edited (${ownId})`).not.toBe(ownId);

    const editArea = await openEditOn(page, ownId);
    // `newest: false`: the reader's row may show only a sliver at the bottom
    // edge, where a pixel of rounding decides whether it still counts as visible.
    await expectInPlace(page, before, "the edit form opening above the reader moved their row", {
      newest: false,
      hold: true,
    });

    await editArea.fill(`edited above the reader: ${"lorem ipsum ".repeat(70)}`);
    await editArea.press("Enter");
    await expect(editArea).toBeHidden({ timeout: 5_000 });
    await expect(page.locator(`[id="${ownId}"]`)).toContainText("edited above the reader", { timeout: 5_000 });
    await expect
      .poll(async () => (await ownHeight()) - heightBefore, {
        timeout: 5_000,
        message: "premise: the edit should make its row materially taller",
      })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);
    await expectInPlace(page, before, "an edit above the reader moved their row", { newest: false, hold: true });
  });
});

/// Open the edit form on the own message whose row is `id`: its kebab on touch,
/// its hover actions otherwise, as `openOwnMessageEdit` (example-room.ts) does
/// for the first own message. Returns the edit textarea.
async function openEditOn(page: Page, id: string) {
  const row = page.locator(`[id="${id}"]`);
  if (await page.evaluate(() => window.matchMedia("(hover: none)").matches)) {
    await row.getByTestId("message-kebab").click();
    await page.getByTestId("message-action-menu").getByRole("button", { name: /edit/i }).click();
  } else {
    await row.getByTestId("message-bubble").hover();
    await row.getByRole("button", { name: /edit/i }).click();
  }
  const editArea = row.locator('textarea[id^="edit-msg-"]');
  await expect(editArea).toBeVisible({ timeout: 5_000 });
  return editArea;
}

/// An anchor-bearing history row: a message, a date separator or an event
/// summary. `top`/`bottom` are relative to the container's top edge, so a row
/// with `bottom < 0` is entirely above the viewport.
type AnchorRow = { id: string; text: string; top: number; bottom: number };

/// Every anchor-bearing row, in DOM order. The deletion fixtures need the real
/// order to know which rows border the reader, so this reads the rows the scroll
/// model can anchor on rather than only the messages.
function anchorRows(page: Page): Promise<AnchorRow[]> {
  return page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container")!;
    const top = c.getBoundingClientRect().top;
    return Array.from(c.querySelectorAll<HTMLElement>("#chat-content [data-anchor-row]")).map(
      (row) => {
        const r = row.getBoundingClientRect();
        return {
          id: row.id,
          text: (row.textContent ?? "").trim().slice(0, 40),
          top: r.top - top,
          bottom: r.bottom - top,
        };
      },
    );
  });
}

/// The message rows among `rows` whose text matches `re`, in order.
function messageRows(rows: AnchorRow[], re: RegExp): AnchorRow[] {
  return rows.filter((row) => row.id.startsWith("msg-") && re.test(row.text));
}

/// The rows `fillHistory` delivered, in order, by their `filler N:` text.
const fillerRows = (rows: AnchorRow[]) => messageRows(rows, /^filler \d+:/);

/// Scroll so the message row `id` is the newest one visible, its top `peek` px
/// above the container's bottom edge, and wait for the scroll to land.
async function parkOnRow(page: Page, id: string, peek = 60): Promise<RowPosition> {
  const target = await page.evaluate(
    ({ id, peek }) => {
      const c = document.getElementById("chat-scroll-container")!;
      const row = document.getElementById(id)!;
      const offset = row.getBoundingClientRect().top - c.getBoundingClientRect().top;
      return c.scrollTop + offset + peek - c.clientHeight;
    },
    { id, peek },
  );
  expect(target, "premise: the parking offset is inside the history").toBeGreaterThan(0);
  await readerParksAt(page, target);
  await afterLayoutSettles(page);
  expect(
    await distanceFromBottom(page),
    "premise: the reader should be parked above the end",
  ).toBeGreaterThan(PARKED_ABOVE_END_PX);
  const newest = await newestVisibleMessage(page);
  expect(newest?.id, "premise: the aimed-at row should be the newest visible one").toBe(id);
  return newest!;
}

/// Remove messages by row id in one state change, and require every one to have
/// named a message and to be gone from the page.
async function removeMessages(page: Page, ids: string[]) {
  const unmatched = await callRiverTest(page, "removeMessages", ids);
  expect(unmatched, "premise: every id should have named a message").toEqual([]);
  await expect
    .poll(
      () => page.evaluate((ids) => ids.filter((id) => document.getElementById(id)).length, ids),
      { timeout: 5_000, message: "premise: every removed message should leave the page" },
    )
    .toBe(0);
}

/// The reader's `scrollTop` drifting from `top`, for the case with no row left
/// to compare against.
async function offsetDrift(page: Page, top: number): Promise<number> {
  return Math.abs((await scrollTop(page)) - top);
}

// `capture` reads the rows through a live `anchor-row` class collection, while
// restore and these specs find them by `data-anchor-row`. A row with one and not
// the other would be skipped by capture or unfindable by restore.
test.describe("Anchor rows", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // DOM membership only, no layout or input: one engine is enough.
  test("the anchor-row class and data-anchor-row mark the same rows in the same order", { tag: "@chromium-only" }, async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    // No example room seeds a join event, so add one between two messages.
    await deliver(page, "before the join event");
    await callRiverTest(page, "appendJoinEvent");
    await expect(page.locator("#chat-content [data-anchor-row][data-item-key]")).not.toHaveCount(0, {
      timeout: 5_000,
    });
    await deliver(page, "after the join event");

    const { byAttr, byClass } = await page.evaluate(() => {
      const kind = (row: Element) =>
        row.id.startsWith("msg-")
          ? "message"
          : row.hasAttribute("data-item-key")
            ? "event"
            : "separator";
      const read = (selector: string) =>
        Array.from(document.querySelectorAll(selector)).map((row) => ({
          key: row.getAttribute("data-anchor-row"),
          kind: kind(row),
        }));
      return {
        byAttr: read("#chat-content [data-anchor-row]"),
        byClass: read("#chat-content .anchor-row"),
      };
    });
    expect(
      new Set(byAttr.map((row) => row.kind)),
      "premise: the room shows a date separator, an event summary and messages",
    ).toEqual(new Set(["separator", "event", "message"]));
    expect(byClass, "the anchor-row class and data-anchor-row disagree").toEqual(byAttr);
  });
});

// Content that changes ABOVE or AT the reader without them scrolling: a
// moderator deleting messages, several messages dropped in one update, an image
// that finishes loading late. With `overflow-anchor: none`, nothing but the
// scroll model holds the reader's text still (#507).
//
// The deletions use a test hook that only re-renders without the messages; it
// says nothing about whether a removal would be authorized. Every fixture is
// plain filler messages (one row each, no reactions or replies), with enough
// history kept below the reader that the offsets involved stay reachable.
test.describe("The reader's place survives removed and late content (#507)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("deleting the reading anchor goes to the latest message, and the next arrival is preserved @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, 24);
    const deleted = fillerRows(await anchorRows(page))[11];
    await parkOnRow(page, deleted.id);

    const rows = await anchorRows(page);
    const at = rows.findIndex((row) => row.id === deleted.id);
    const neighbor = rows[at - 1];
    expect(
      neighbor.id,
      "premise: the anchor-bearing row right above the reader's message is a message",
    ).toMatch(/^msg-/);

    await removeMessages(page, [deleted.id]);
    expect(
      await page.evaluate((id) => document.getElementById(id) !== null, neighbor.id),
      "premise: the row above the deleted anchor is still in the history",
    ).toBe(true);
    await expectSettledAtBottom(
      page,
      "deleting the reading anchor should place at the latest message, not on the row above it",
    );
    const landed = await newestVisibleMessage(page);
    expect(landed, "premise: a message is visible at the landing").not.toBeNull();
    expect(landed!.id, "the landing is not the surviving neighbor").not.toBe(neighbor.id);

    await deliver(
      page,
      `arrival after the reading anchor was deleted ${"y".repeat(400)}`,
    );
    await expectInPlace(page, landed!, "an arrival after the landing moved the reader", {
      newest: false,
      hold: true,
    });
    expect(
      await distanceFromBottom(page),
      "the arrival after the landing should stay below the view",
    ).toBeGreaterThan(PARKED_ABOVE_END_PX);
  });

  test("several messages removed above the reader in one update leave their message in place @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, 24);
    const before = await parkOnRow(page, fillerRows(await anchorRows(page))[15].id);

    const doomed = fillerRows(await anchorRows(page)).slice(1, 6);
    expect(
      Math.max(...doomed.map((row) => row.bottom)),
      "premise: every removed message is entirely above the viewport",
    ).toBeLessThan(0);
    const top = await scrollTop(page);
    const heightBefore = await historyHeight(page);
    await removeMessages(page, doomed.map((row) => row.id));
    const removed = heightBefore - (await historyHeight(page));
    expect(removed, "premise: the removal takes a material height off the history").toBeGreaterThan(
      PARKED_ABOVE_END_PX,
    );
    expect(top - removed, "premise: the corrected offset is reachable").toBeGreaterThan(
      IN_PLACE_TOLERANCE_PX,
    );

    await expectInPlace(page, before, "messages removed above the reader moved their message", {
      hold: true,
    });
    await deliver(page, "arrival after the bulk removal");
    await expectInPlace(page, before, "an arrival after the bulk removal moved the reader", {
      hold: true,
    });
  });

  // No message-state change: only the image's own load re-lays the history out.
  test("an image that loads late above the reader leaves their message in place @fractional-geometry", async ({
    page,
  }) => {
    let held: import("@playwright/test").Route | null = null;
    await page.route("**/late-image.svg", (route) => {
      held = route;
    });
    try {
      await openRoomAtBottom(page, "Team Chat Room");
      await callRiverTest(page, "appendMessage", "late image ![late](/late-image.svg)");
      await expect(page.locator('img[src$="late-image.svg"]')).toBeAttached({ timeout: 5_000 });
      await expect.poll(() => held !== null, { message: "premise: the image was requested" }).toBe(true);
      await fillHistory(page, 12);
      const before = await parkOnRow(page, fillerRows(await anchorRows(page))[6].id);

      const imageRow = () =>
        page.evaluate(() => {
          const c = document.getElementById("chat-scroll-container")!;
          const img = c.querySelector<HTMLImageElement>('img[src$="late-image.svg"]')!;
          const row = img.closest<HTMLElement>('[id^="msg-"]')!;
          const r = row.getBoundingClientRect();
          return {
            bottom: r.bottom - c.getBoundingClientRect().top,
            height: r.height,
            loaded: img.complete && img.naturalHeight > 0,
          };
        });
      const pending = await imageRow();
      expect(pending.loaded, "premise: the image has not loaded yet").toBe(false);
      expect(pending.bottom, "premise: the image's row is above the viewport").toBeLessThan(0);

      await held!.fulfill({
        contentType: "image/svg+xml",
        body: '<svg xmlns="http://www.w3.org/2000/svg" width="320" height="240"><rect width="320" height="240" fill="#888"/></svg>',
      });
      held = null;
      await expect
        .poll(async () => (await imageRow()).height - pending.height, {
          timeout: 5_000,
          message: "premise: the loaded image should make its row materially taller",
        })
        .toBeGreaterThan(PARKED_ABOVE_END_PX);

      await expectInPlace(page, before, "an image loading above the reader moved their message", {
        hold: true,
      });
      await deliver(page, "arrival after the late image");
      await expectInPlace(page, before, "an arrival after the late image moved the reader", {
        hold: true,
      });
    } finally {
      // Never leave the request hanging, even when an assertion failed first.
      await (held as import("@playwright/test").Route | null)?.abort().catch(() => {});
    }
  });
});

/// The numbers the scroll model's layout signature describes (content and
/// container sizes), plus the scroll range, which it does not.
type Shape = {
  top: number;
  max: number;
  contentHeight: number;
  contentWidth: number;
  viewport: number;
  viewportWidth: number;
};

/// Set the height of a test-only box hanging absolutely positioned below the
/// history's content, or remove it with `null`. It adds scrollable overflow
/// without resizing the content wrapper or the container, the way an open
/// popover near the end of the history does. Returns the shape before and
/// after, the after read synchronously so any clamp has already happened.
function setOverhang(page: Page, height: number | null): Promise<{ before: Shape; after: Shape }> {
  return page.evaluate((height) => {
    const c = document.getElementById("chat-scroll-container")!;
    const content = document.getElementById("chat-content")!;
    const shape = () => ({
      top: c.scrollTop,
      max: c.scrollHeight - c.clientHeight,
      contentHeight: content.getBoundingClientRect().height,
      contentWidth: content.clientWidth,
      viewport: c.clientHeight,
      viewportWidth: c.clientWidth,
    });
    const before = shape();
    let box = document.getElementById("test-overhang");
    if (height === null) {
      box?.remove();
    } else {
      if (!box) {
        // Positioned so the box's overflow belongs to the scroll container.
        content.style.position = "relative";
        box = document.createElement("div");
        box.id = "test-overhang";
        box.style.cssText = "position:absolute;top:100%;left:0;width:1px;pointer-events:none;";
        content.appendChild(box);
      }
      box.style.height = `${height}px`;
    }
    return { before, after: shape() };
  }, height);
}

/// The signature's dimensions did not change between `a` and `b`.
function expectSameShape(a: Shape, b: Shape, why: string) {
  for (const key of ["contentHeight", "contentWidth", "viewport", "viewportWidth"] as const) {
    expect(Math.abs(a[key] - b[key]), `${why}: ${key} ${a[key]} -> ${b[key]}`).toBeLessThanOrEqual(0.5);
  }
}

// The browser clamps `scrollTop` when the scroll range shrinks below it. A clamp
// is not the reader moving, however far it goes and whether or not anything the
// layout signature describes changed; a reader's own move is theirs, even right
// after overflow the signature does not describe has changed.
test.describe("A clamp to the end is not the reader", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("widening a history whose long rows above the reader shrink keeps their message @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    // Narrow first, so the long rows wrap tall; widening then shrinks them.
    await page.evaluate(() => {
      document.getElementById("chat-content")!.style.maxWidth = "360px";
    });
    await afterLayoutSettles(page);
    for (let i = 0; i < 4; i++) {
      await deliver(page, `long ${i}: ${"lorem ipsum dolor sit amet ".repeat(24)}`);
    }
    for (let i = 0; i < 10; i++) {
      await deliver(page, `short ${i}`);
    }
    await readerScrollsToEnd(page);
    // Among the short rows, which barely rewrap: the rows below the reader keep
    // their height, so the reader's message can go back to its gap without
    // running into the new end.
    const shortRows = messageRows(await anchorRows(page), /^short \d+/);
    expect(shortRows.length, "premise: the short rows are on the page").toBe(10);
    const before = await parkOnRow(page, shortRows[shortRows.length - 4].id);

    const widened = await page.evaluate(
      ({ id, gap }) => {
        const c = document.getElementById("chat-scroll-container")!;
        const content = document.getElementById("chat-content")!;
        const row = document.getElementById(id)!;
        const top = c.scrollTop;
        content.style.maxWidth = "";
        // Read synchronously: the layout and its clamp happen here.
        const clamped = c.scrollTop;
        const max = c.scrollHeight - c.clientHeight;
        const gapNow = c.getBoundingClientRect().bottom - row.getBoundingClientRect().top;
        return { clamp: top - clamped, atEnd: max - clamped, restoreTo: clamped + gap - gapNow, max };
      },
      before,
    );
    expect(
      widened.clamp,
      `premise: the browser's clamp is larger than the layout allowance (${JSON.stringify(widened)})`,
    ).toBeGreaterThan(LAYOUT_SHIFT_ALLOWANCE_PX);
    expect(Math.abs(widened.atEnd), "premise: the clamp lands at the new end").toBeLessThanOrEqual(1);
    expect(
      widened.restoreTo,
      `premise: the reader's message can go back to its gap without a clamp (${JSON.stringify(widened)})`,
    ).toBeLessThanOrEqual(widened.max - IN_PLACE_TOLERANCE_PX);

    await expectInPlace(page, before, "the widening's clamp was read as the reader moving to the end", {
      hold: true,
    });
    await deliver(page, "arrival after the widening");
    await expectInPlace(page, before, "an arrival after the widening moved the reader", {
      hold: true,
    });
    expect(await distanceFromBottom(page), "the view was pulled to the end").toBeGreaterThan(PARKED_ABOVE_END_PX);
  });

  test("a small reader scroll after an overhang grew is kept", async ({ page }) => {
    const SMALL_MOVE_PX = 150;
    await parkAt(page, "half");
    await setOverhang(page, 0);
    await afterLayoutSettles(page);
    const grown = await setOverhang(page, 800);
    expect(
      grown.after.max - grown.before.max,
      "premise: the overhang adds scroll range",
    ).toBeGreaterThan(LAYOUT_SHIFT_ALLOWANCE_PX);
    expectSameShape(grown.before, grown.after, "premise: the overhang resizes nothing the signature describes");
    await afterLayoutSettles(page);
    expect(await scrollTop(page), "premise: the overhang does not move the view").toBeCloseTo(grown.before.top, 0);

    const target = grown.after.top - SMALL_MOVE_PX;
    const landed = await readerScrollsTo(page, target);
    expect(Math.abs(landed - target), "premise: the reader's scroll lands where it was aimed").toBeLessThanOrEqual(1);
    await expectDriftWithin(
      page,
      () => offsetDrift(page, target),
      "the reader's small scroll was put back as if the overhang had moved it",
      { hold: true },
    );
    const parked = await savedRow(page);
    await deliver(page, "arrival after the overhang grew");
    await expectInPlace(page, parked, "an arrival moved the reader after their small scroll", { hold: true });
  });

  // Parked with and without a wheel. After a synthetic `wheel`, WebKit delivers
  // every later `scroll` event on the element twice (measured on a bare scroller
  // outside the app, 2026-10-01; Chromium and Firefox send one), so the clamp's
  // event comes again once the restore has recorded the clamped view. That copy
  // must read as an echo, not as the reader arriving at the end.
  //
  // The reader's gap is out of reach while the range is short; it stays saved,
  // and the arrival that makes it reachable again puts their message back.
  for (const wheel of [false, true]) {
    test(`removing overflow clamps a parked reader, and the arrival that restores the range puts their message back${wheel ? " (parked with a wheel)" : ""} @fractional-geometry`, async ({
      page,
    }) => {
      const OVERHANG_PX = 700;
      // Into the overhang by more than the layout allowance, so this is a clamp no
      // allowance could excuse, and still well above the end.
      const INTO_OVERHANG_PX = 300;
      await openRoomAtBottom(page, "Team Chat Room");
      await fillHistory(page, 12);
      const created = await setOverhang(page, 0);
      await afterLayoutSettles(page);
      await setOverhang(page, OVERHANG_PX);
      await afterLayoutSettles(page);

      const contentEnd = created.before.max;
      const target = contentEnd + INTO_OVERHANG_PX;
      const landed = await readerScrollsTo(page, target, { wheel });
      expect(Math.abs(landed - target), "premise: the reader's scroll lands where it was aimed").toBeLessThanOrEqual(1);
      await afterLayoutSettles(page);
      expect(await distanceFromBottom(page), "premise: the reader is parked above the end").toBeGreaterThan(
        PARKED_ABOVE_END_PX,
      );
      const before = await savedRow(page);

      const removed = await setOverhang(page, null);
      expectSameShape(removed.before, removed.after, "premise: removing the overhang resizes nothing the signature describes");
      expect(
        removed.before.top - removed.after.top,
        "premise: removing the overhang clamps the view",
      ).toBeGreaterThan(LAYOUT_SHIFT_ALLOWANCE_PX);
      expect(
        Math.abs(removed.after.max - removed.after.top),
        "premise: the clamp lands at the new end",
      ).toBeLessThanOrEqual(1);
      // The old gap is out of reach until the history grows again: the view stays
      // where the clamp put it.
      await expectDriftWithin(
        page,
        () => offsetDrift(page, removed.after.top),
        "the clamped view moved",
        { hold: true },
      );

      // Tall enough to make the old gap reachable again, with room to spare.
      const marker = "tall arrival after the overhang went";
      await callRiverTest(page, "appendMessage", TALL(marker, 30));
      await expect(page.getByText(marker).last()).toBeAttached({ timeout: 5_000 });
      const arrivalHeight = await page.evaluate(
        (marker) => window.__riverScroll.rowWithText(marker)!.getBoundingClientRect().height,
        marker,
      );
      expect(
        arrivalHeight,
        "premise: the arrival restores more range than the clamp took, with room to spare",
      ).toBeGreaterThan(INTO_OVERHANG_PX + 2 * PARKED_ABOVE_END_PX);
      await expectInPlace(page, before, "the clamp replaced the parked reader's saved row", {
        newest: false,
        hold: true,
      });
      expect(await distanceFromBottom(page), "the view was pulled to the end").toBeGreaterThan(PARKED_ABOVE_END_PX);
    });
  }
});

// freenet/river#723: the reader scrolls back down to the newest message, and an
// arrival lands BEFORE that scroll's event is delivered. If the arrival's
// restore went on the stale recorded position, it would put the view back where
// the reader just left, and their scroll to the end would be lost.
test.describe("An arrival ahead of the reader's scroll event (#723)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a tall arrival that beat the reader's scroll event to the end keeps where they scrolled to (controlled order: patch → scroll)", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await afterLayoutSettles(page);
    expect(
      await distanceFromBottom(page),
      "premise: the reader is parked well above the end",
    ).toBeGreaterThan(4 * PARKED_ABOVE_END_PX);

    const marker = "tall arrival ahead of the scroll event";
    // One sequence, in a frame callback: the scroll's event is due in the NEXT
    // frame, while the hook's delivery runs on a timer before it. Which lands
    // first is the engine's call (Linux CI Firefox delivers the scroll first), so
    // a capture-phase gate holds the native scroll events back from the app
    // until the arrival's row has landed, then hands it one marked `scroll`.
    const race = await page.evaluate(
      ({ text, marker }) =>
        new Promise<{ order: string[]; arrivalHeight: number; held: number; reached: RowPosition | null }>(
          (resolve) => {
            const c = document.getElementById("chat-scroll-container")!;
            const order: string[] = [];
            let arrivalHeight = 0;
            let held = 0;
            let reached: RowPosition | null = null;
            const released = new WeakSet<Event>();
            const gate = (e: Event) => {
              if (e.target !== c || released.has(e) || order.includes("patch")) return;
              e.stopImmediatePropagation();
              held += 1;
            };
            window.addEventListener("scroll", gate, true);
            const stopPatch = window.__riverScroll.patchLanded(marker, (row) => {
              order.push("patch");
              arrivalHeight = row.getBoundingClientRect().height;
              const release = new Event("scroll");
              released.add(release);
              c.dispatchEvent(release);
              done();
            });
            const onScroll = () => {
              if (!order.includes("scroll")) order.push("scroll");
              done();
            };
            const timer = setTimeout(() => finish(), 3_000);
            function done() {
              if (order.length === 2) finish();
            }
            function finish() {
              clearTimeout(timer);
              stopPatch();
              window.removeEventListener("scroll", gate, true);
              c.removeEventListener("scroll", onScroll);
              resolve({ order, arrivalHeight, held, reached });
            }
            c.addEventListener("scroll", onScroll);
            requestAnimationFrame(() => {
              c.scrollTop = c.scrollHeight;
              // Where the reader actually is, before anything else lands.
              reached = window.__riverScroll.newestVisible(c);
              window.__riverTest!.appendMessage(text);
            });
          },
        ),
      { text: TALL(marker), marker },
    );
    expect(
      race.order,
      `premise: the app sees the arrival's patch before the scroll event (observed: ${race.order.join(" → ") || "nothing"}; native scroll events held back: ${race.held})`,
    ).toEqual(["patch", "scroll"]);
    expect(
      race.arrivalHeight,
      "premise: the arrival is taller than the tolerance could hide",
    ).toBeGreaterThan(PARKED_ABOVE_END_PX);
    expect(race.reached, "premise: a message was visible at the end").not.toBeNull();

    await expectNotFollowed(
      page,
      race.reached!,
      `the reader scrolled to the end and the arrival ahead of their scroll event did not keep their row (order: ${race.order.join(" → ")})`,
    );
    await deliver(page, "arrival after the race");
    await expectNotFollowed(page, race.reached!, "the arrival after the race moved the reader's row");
  });
});

/// How far above the end the scroll-to-latest animation must still be when the
/// room switch interrupts it, so the interruption is mid-flight, not at the end.
const NAV_MID_FLIGHT_PX = 300;

/// Scroll the reader to the top, press the scroll-to-latest button, and switch
/// to `room` from inside the first scroll event of its native animation that has
/// visibly started and is still more than NAV_MID_FLIGHT_PX above the end. In
/// the event, so nothing else runs between the check and the switch; the app's
/// own listener was installed first, so that frame is already recorded.
async function switchRoomMidNavigation(page: Page, room: string) {
  await readerScrollsWithoutGesture(page, 0);
  await afterLayoutSettles(page);
  await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({ timeout: 5_000 });
  const result = await page.evaluate(
    ({ room, midFlight }) =>
      new Promise<{ fired: boolean; distance: number; frames: number; why: string }>((resolve) => {
        const c = document.getElementById("chat-scroll-container")!;
        const start = c.scrollTop;
        let frames = 0;
        const finish = (r: { fired: boolean; distance: number; why: string }) => {
          c.removeEventListener("scroll", onScroll);
          clearTimeout(timer);
          resolve({ ...r, frames });
        };
        const timer = setTimeout(() => finish({ fired: false, distance: NaN, why: "no animation frame came" }), 5_000);
        const onScroll = () => {
          frames++;
          const top = c.scrollTop;
          const distance = c.scrollHeight - c.clientHeight - top;
          if (top < start + 50) return;
          if (distance <= midFlight) {
            finish({ fired: false, distance, why: "the animation was already near the end" });
            return;
          }
          window.__riverTest!.switchRoom(room);
          finish({ fired: true, distance, why: "" });
        };
        c.addEventListener("scroll", onScroll);
        (document.querySelector('[data-testid="scroll-to-bottom"]') as HTMLElement).click();
      }),
    { room, midFlight: NAV_MID_FLIGHT_PX },
  );
  expect(
    result.fired,
    `premise: the switch should land mid-flight (${result.why}; ${result.frames} frames, ${result.distance}px above the end)`,
  ).toBe(true);
}

/// In one task: move the view to `top` and switch to `room`, before any scroll
/// event. No synthetic wheel: after one, WebKit sends this scroll's `scrollend`
/// ahead of its `scroll` event.
async function scrollThenSwitchRoom(page: Page, top: number, room: string) {
  await afterLayoutSettles(page);
  await page.evaluate(
    ({ top, room }) =>
      new Promise<void>((resolve) => {
        requestAnimationFrame(() => {
          document.getElementById("chat-scroll-container")!.scrollTop = top;
          window.__riverTest!.switchRoom(room);
          setTimeout(resolve, 600);
        });
      }),
    { top, room },
  );
}

// A room visited for the first time this session opens at its newest message,
// whatever the room being left was doing. Revisits and own sends live in their
// own spec.
test.describe("A room switched to for the first time opens at its newest message", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a switch mid-way through scroll-to-latest opens the new room at its newest message, and the animation does not carry over", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room", DEEP_ROOM_PATH);
    await fillHistory(page, 16);
    await switchRoomMidNavigation(page, "Deep History Room");
    await expect(page.getByRole("heading", { name: "Deep History Room" })).toBeVisible();
    await expectSettledAtBottom(page, "the room switched to mid-animation did not open at its newest message");
    await afterLayoutSettles(page);
    // The old room's animation, still running on the same container, would
    // carry the view away from the new room's placement.
    const placed = await savedRow(page);
    await expectInPlace(page, placed, "the old room's animation carried over into the new room", { hold: true });
    await expectSettledAtBottom(page, "the new room's view left its newest message");
    await deliver(page, "arrival in the new room");
    await expectNotFollowed(page, placed, "an arrival in the new room moved its placement");
  });

  test("a switch with the reader's scroll still pending opens the new room at its newest message, then preserves it", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room", DEEP_ROOM_PATH);
    await fillHistory(page);
    await scrollThenSwitchRoom(page, await endMinus(page, 40), "Deep History Room");
    await expect(page.getByRole("heading", { name: "Deep History Room" })).toBeVisible();
    await expectSettledAtBottom(page, "the new room did not open at its newest message");
    await afterLayoutSettles(page);
    const placed = await savedRow(page);
    await deliver(page, "arrival in the new room");
    await expectNotFollowed(page, placed, "an arrival in the new room moved its placement");
  });
});

// Regression tests for freenet/river#501: the #498 windowed tail slid its
// start index forward on every arrival, removing the oldest rendered rows in
// the same patch that appended the new message. Browser scroll anchoring
// rewrote scrollTop to hold the visible content still, and the old code read
// that as the reader moving; every room the old suite seeded (~15-20 items vs a
// 60-item window) kept passing on the pre-window code path.
//
// Every test here therefore asserts its PREMISE first — the backfill sentinel
// is attached and the rendered row count is a windowed tail, not the whole
// fixture — so a fixture change that shrinks the room below the window makes
// these fail loudly instead of quietly regressing into small-room tests.
//
// The fixture: `?deep-history-room=1` adds a room of 80 alternating-author
// messages (alternation makes messages == display items, so >60 items is a
// guarantee) plus the ~13 standard fixture messages. The default fixture is
// untouched; the describes above still exercise the small-room path.
test.describe("Windowed history keeps the reader's row (#501)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  const DEEP_ROOM = "Deep History Room";
  /// Seeded exactly at its max_recent_messages cap: every delivered arrival
  /// drains the oldest message, shifting every item index (#505 blocker 1).
  const CAPPED_ROOM = "Capped History Room";

  /// The premise all these tests stand on: the windowed render path is
  /// actually active. Without this, a fixture or window-size change could
  /// turn every test below into a duplicate of the small-room suite — the
  /// exact coverage gap that let #501 ship green.
  async function expectWindowedRenderActive(page: Page) {
    await expect(
      page.locator("#top-backfill-sentinel"),
      "premise: the backfill sentinel must be attached — if the room fits in " +
        "the window, nothing here exercises #501's code path"
    ).toHaveCount(1);
    const rows = await renderedRowCount(page);
    expect(
      rows,
      "premise: the deep room must render a windowed TAIL (~60 items plus " +
        "separators), not the whole fixture — a full render means the window " +
        "backfilled behind the reader's back or the fixture shrank"
    ).toBeLessThan(75);
    expect(
      rows,
      "premise: the windowed tail itself should be on the page"
    ).toBeGreaterThan(40);
  }

  /// Tag the first history row fully inside the container's viewport and
  /// return its viewport-relative top. scrollTop alone cannot see a window
  /// slide: with scroll anchoring disabled, dropping rows above the viewport
  /// leaves scrollTop untouched and shifts the CONTENT under it — the reader
  /// watches their message jump away while every offset reads steady. The
  /// probed row's rect is what catches that.
  async function tagVisibleRow(page: Page, flag: string): Promise<number | null> {
    return page.evaluate(([f, rowsSelector]) => {
      const container = document.getElementById("chat-scroll-container")!;
      const cRect = container.getBoundingClientRect();
      const rows = container.querySelectorAll(rowsSelector);
      for (const row of rows) {
        const r = row.getBoundingClientRect();
        if (r.top >= cRect.top && r.bottom <= cRect.bottom) {
          (row as any)[f] = true;
          return r.top;
        }
      }
      return null;
    }, [flag, HISTORY_ROWS]);
  }

  /// The tagged row's current viewport-relative top, or null if it left the
  /// DOM (i.e. the window slid out from under it).
  async function taggedRowTop(page: Page, flag: string): Promise<number | null> {
    return page.evaluate(([f, rowsSelector]) => {
      const rows = document.querySelectorAll(rowsSelector);
      for (const row of rows) {
        if ((row as any)[f]) {
          return row.getBoundingClientRect().top;
        }
      }
      return null;
    }, [flag, HISTORY_ROWS]);
  }

  test("a burst of arrivals at the end of a windowed room keeps the reader's row; their own return to the end trims the window and paging still works", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);
    await afterLayoutSettles(page);
    const before = await savedRow(page);
    const initialRows = await renderedRowCount(page);

    // The #501 shape: each arrival grows the window while the reader sits at
    // the end. Delivered arrivals alternate authors (see `test_author` in
    // test_hooks.rs), so each one is its own display item and the loop
    // exercises the windowing arithmetic six times, not once.
    for (let i = 1; i <= 6; i++) {
      await deliver(page, `windowed arrival ${i}`);
      await expectInPlace(page, before, `arrival ${i} in a windowed room moved the reader's row`, {
        newest: false,
      });
    }
    await expectNotFollowed(page, before, "the arrivals in a windowed room moved the reader's row");
    expect(
      await renderedRowCount(page),
      "premise: the arrivals grew the rendered window past its initial size",
    ).toBeGreaterThanOrEqual(initialRows + 6);

    // A view resting AT the end is the one moment the bounded-window trim is
    // invisible: the reader scrolls there themselves, and the window goes back
    // toward its initial size with the row they landed on held still. Polled
    // because the trim lands asynchronously. The deterministic guard on the
    // trim's gating is the source pin `the_trim_stays_gated_at_the_bottom` in
    // conversation.rs.
    await readerScrollsToEnd(page);
    const landed = await savedRow(page);
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message: "the reader's own return to the end did not trim the window back toward its initial size",
      })
      .toBeLessThan(67);
    await expectInPlace(page, landed, "the trim moved the row the reader landed on", { hold: true });

    // And paging back still reveals older history from the trimmed window.
    const trimmedRows = await renderedRowCount(page);
    await page.evaluate(() => {
      document.getElementById("chat-scroll-container")!.scrollTop = 0;
    });
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message: "reaching the top after the trim did not backfill older rows",
      })
      .toBeGreaterThan(trimmedRows + 40);
  });

  test("arrivals do not move a reader parked in a windowed room's history", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // Park mid-history: far enough from the bottom to be clearly up in the
    // history, far enough from the top not to trigger a backfill.
    const mid = Math.floor((await historyHeight(page)) / 2);
    await readerScrollsTo(page, mid);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);

    const probe = await tagVisibleRow(page, "__riverProbe501");
    expect(
      probe,
      "premise: a rendered row should be visible mid-history"
    ).not.toBeNull();

    for (let i = 1; i <= 3; i++) {
      await deliver(page, `parked windowed arrival ${i}`);
    }
    await expectStaysPut(
      page,
      "arrivals in a windowed room moved a parked reader's scroll offset"
    );

    const after = await taggedRowTop(page, "__riverProbe501");
    expect(
      after,
      "the probed row left the DOM — the window slid out from under a parked reader"
    ).not.toBeNull();
    expect(
      Math.abs(after! - probe!),
      "content shifted under a parked reader when arrivals landed"
    ).toBeLessThanOrEqual(2);
  });

  test("arrivals do not crawl a parked reader in an at-cap room", async ({
    page,
  }) => {
    // The at-cap room sits EXACTLY at max_recent_messages, so every arrival
    // drains the oldest message and shifts every item index down — the
    // steady state of every busy production room. A positional window anchor
    // then swaps the head's IDENTITY one item per arrival: the top rendered
    // row is removed in the same patch that appends the new one, and the
    // parked reader crawls upward one row height each time (#505 blocker 1).
    // The identity anchor must hold the head fixed instead.
    await openRoomAtBottom(page, CAPPED_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    const mid = Math.floor((await historyHeight(page)) / 2);
    await readerScrollsTo(page, mid);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);

    const probe = await tagVisibleRow(page, "__riverProbeAtCap");
    expect(
      probe,
      "premise: a rendered row should be visible mid-history"
    ).not.toBeNull();

    for (let i = 1; i <= 4; i++) {
      await deliver(page, `at-cap arrival ${i}`);
    }
    await expectStaysPut(
      page,
      "at-cap arrivals moved a parked reader's scroll offset"
    );

    const after = await taggedRowTop(page, "__riverProbeAtCap");
    expect(
      after,
      "the probed row left the DOM — the at-cap window slid in content space"
    ).not.toBeNull();
    expect(
      Math.abs(after! - probe!),
      "content crawled under a parked reader as at-cap arrivals pruned the history"
    ).toBeLessThanOrEqual(2);
  });

  test("a batched at-cap drain does not crawl a parked reader (re-keyed head group)", async ({
    page,
  }) => {
    // The capped room's fillers come in same-author PAIRS, so its display
    // groups hold two messages — the dominant production shape. A batch of
    // 61 arrivals drains 61 messages: ~30 whole pairs past the window head,
    // plus one half-pair that RE-KEYS the group at the drain boundary (a
    // group's key is its first message's id). The visible message is still
    // the reading anchor, so the window must keep that message even though
    // the old head key is gone.
    //
    // No scrollTop-stability assertion here, deliberately: the compensation
    // MOVES scrollTop to hold the CONTENT still. The probed row's rect is
    // the thing that must not move.
    await openRoomAtBottom(page, CAPPED_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // Park just far enough up to be clearly above the end, NOT mid-history:
    // the 61-message batch drains ~30 display items off the FRONT of a 74-item
    // room, so a row tagged mid-history is inside the pruned range and
    // legitimately leaves the DOM — which row exactly depends on per-engine row
    // heights, so tagging there is flaky by construction rather than by timing.
    // The rows just above the fold are the newest ones; they survive the drain,
    // and holding THEM still is the property under test.
    //
    // Deliberately NO wait before delivering: the batch lands while the
    // reader's scroll event may still be pending, so this also covers a stale
    // recorded position. The restore must take that scroll in first, and the
    // test would go quiet about it if it waited the event out.
    const parkedAt = Math.max(0, await endMinus(page, 400));
    await readerScrollsTo(page, parkedAt);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);

    const probe = await tagVisibleRow(page, "__riverProbeBatch");
    expect(
      probe,
      "premise: a rendered row should be visible above the fold"
    ).not.toBeNull();

    const beforeBatch = await renderedRowCount(page);
    await callRiverTest(page, "appendMessages", 61);
    // The batch landed and the SURVIVING remainder of the old window is still
    // rendered (the arrivals alone add 61 rows; losing the survivors would
    // shrink the count back toward the window size).
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message:
          "premise: the batch should land with the surviving window rows kept",
      })
      .toBeGreaterThan(beforeBatch + 30);
    // Let the restore's scroll events land before measuring.
    await page.waitForTimeout(300);

    const after = await taggedRowTop(page, "__riverProbeBatch");
    expect(
      after,
      "the probed row left the DOM — the window lost the surviving rows " +
        "under a parked reader"
    ).not.toBeNull();
    expect(
      Math.abs(after! - probe!),
      "content moved under a parked reader across a batched at-cap drain"
    ).toBeLessThanOrEqual(3);
  });

  test("backfill paging reveals older history and keeps the reader's place", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);
    const initialRows = await renderedRowCount(page);

    // Scroll to the very top and tag the current head ITEM row IN THE SAME
    // browser task — the sentinel's IntersectionObserver fires in a later
    // task, so the tag always lands before the backfill. The head's date
    // SEPARATOR is not a valid probe: when older rows land above it, the day
    // no longer starts at the old head, so that row legitimately leaves the
    // DOM. The item row itself survives — item identity is exactly what the
    // window anchors on.
    const probeTop = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      c.scrollTop = 0;
      const row = c.querySelector('[data-testid="conversation-history"] > [data-item-key]') as HTMLElement;
      (row as any).__riverPagingProbe = true;
      return row.getBoundingClientRect().top;
    });

    // The first growth step reveals a full page of older rows...
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message: "reaching the top should backfill a page of older rows",
      })
      .toBeGreaterThan(initialRows + 40);

    // ...and the restore keeps the row the reader was looking at where it
    // was: the revealed rows land ABOVE, the offset is re-anchored by the
    // container's measured growth.
    const probeAfter = await taggedRowTop(page, "__riverPagingProbe");
    expect(
      probeAfter,
      "the pre-backfill head row must survive a backfill"
    ).not.toBeNull();
    expect(
      Math.abs(probeAfter! - probeTop),
      "a backfill must not move the reader's view"
    ).toBeLessThanOrEqual(3);

    // Repeated paging must reach the very oldest history — a growth step
    // that reveals nothing dead-ends here and the loop times out.
    const oldestFiller = page.getByText("history filler 00", { exact: false });
    for (let i = 0; i < 6 && (await oldestFiller.count()) === 0; i++) {
      const before = await renderedRowCount(page);
      await page.evaluate(() => {
        document.getElementById("chat-scroll-container")!.scrollTop = 0;
      });
      await expect
        .poll(() => renderedRowCount(page), {
          timeout: 5_000,
          message: `paging step ${i} revealed no further rows`,
        })
        .toBeGreaterThan(before);
    }
    expect(
      await oldestFiller.count(),
      "paging back through the whole room must reach the oldest filler"
    ).toBe(1);
  });

  test("backfill still reveals older rows after arrivals grew the window", async ({
    page,
  }) => {
    // #505 blocker 2: arrivals grow an anchored window past its REQUESTED
    // size. A growth step computed from the stale request then resolves to a
    // start the window already renders — zero new rows, no DOM change, the
    // sentinel's IntersectionObserver never re-fires, and paging dead-ends.
    // The growth step must come from the RENDERED size.
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // Park mid-history, so nothing the reader does trims the window back
    // before paging.
    const mid = Math.floor((await historyHeight(page)) / 2);
    await readerScrollsTo(page, mid);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);

    const beforeBatch = await renderedRowCount(page);
    // One batched delivery of more than a whole growth step, in a single
    // state mutation — as a network delta carrying many messages does.
    await callRiverTest(page, "appendMessages", 61);
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message:
          "premise: the batch should grow the anchored window past one whole " +
          "growth step — without that divergence this test exercises nothing",
      })
      .toBeGreaterThan(beforeBatch + 55);

    // Now page up. The FIRST sentinel fire must reveal older rows.
    const beforePaging = await renderedRowCount(page);
    await page.evaluate(() => {
      document.getElementById("chat-scroll-container")!.scrollTop = 0;
    });
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message:
          "the first backfill after an arrival burst revealed no older rows — " +
          "the growth step dead-ended (#505 blocker 2)",
      })
      .toBeGreaterThan(beforePaging + 40);
  });

  test("opening a deep room lands once at its newest message with the initial window, then preserves it", async ({
    page,
  }) => {
    // `openRoomAtBottom` itself asserts the placement; the premise check is
    // what rules out the H2 failure shape, where the backfill sentinel fires
    // from scrollTop 0 before the opening placement and cascades the window
    // over the whole room (the row count would be ~200, not ~62). Note the H2
    // race is timing-dependent in a live browser — this test catches it when
    // it fires, but the deterministic guard is the source pin on the
    // sentinel's `position_ready` mount gate in conversation.rs.
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // And it STAYS there: a late backfill restore racing the opening placement
    // (H3) would park the view at the restore anchor moments later.
    await expectStaysPut(page, "the view moved after the room-open placement");
    await expectSettledAtBottom(
      page,
      "the room should still be at its newest message after settling"
    );

    // The placement happens once: the next arrival is preserved like any other.
    const placed = await savedRow(page);
    await deliver(page, `arrival after opening the deep room: ${"d".repeat(200)}`);
    await expectNotFollowed(page, placed, "an arrival after the deep room's placement moved the reader's row");
  });
});

// On mobile the chat column is `display:none` while the room list or the member
// list is open. Every geometry read is then 0, and none of it is where the
// reader is. The browser keeps `scrollTop` across the hide, so a plain round
// trip comes back in place on its own; these change something while hidden.
test.describe("The hidden mobile chat column", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  const chat = (page: Page) => page.locator("#chat-scroll-container");

  /// Deliver `count` tall-ish arrivals while the chat is hidden.
  async function deliverWhileHidden(page: Page, count: number) {
    for (let i = 1; i <= count; i++) {
      const text = `hidden arrival ${i}: ${"z".repeat(200)}`;
      await callRiverTest(page, "appendMessage", text);
      await expect(page.getByText(text)).toBeAttached({ timeout: 5_000 });
    }
    await afterLayoutSettles(page);
  }

  test("a parked reader keeps their message when rows above it change while hidden @fractional-geometry", async ({
    page,
  }) => {
    // At its cap, so every arrival drains the oldest message above the reader.
    await openRoomAtBottom(page, "Capped History Room", DEEP_ROOM_PATH);
    await readerScrollsTo(page, await endMinus(page, 400));
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(PARKED_ABOVE_END_PX);
    await afterLayoutSettles(page);
    const before = await savedRow(page);

    await hideChat(page, "header-members-button");
    const beforeBatch = await renderedRowCount(page);
    await callRiverTest(page, "appendMessages", 61);
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message: "premise: the batch should land while the chat is hidden",
      })
      .toBeGreaterThan(beforeBatch + 30);
    await afterLayoutSettles(page);

    await revealChat(page, "members-back-button");
    await expectSameMessageInPlace(
      page,
      before,
      "the reader came back to a different place after the rows above them changed",
    );
  });

  test("a reader at the end who hides the chat comes back to the same row, not to the arrivals that landed while hidden", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    expect(await scrollTop(page), "premise: the bottom is well down the history").toBeGreaterThan(400);
    await afterLayoutSettles(page);
    const before = await savedRow(page);

    await hideChat(page, "hamburger-rooms-button");
    await deliverWhileHidden(page, 3);

    // Back through the room list, choosing the room already open.
    await selectListedRoom(page, "Team Chat Room");
    await expect(chat(page)).toBeVisible();
    await expectNotFollowed(page, before, "the reveal showed the arrivals' end instead of the reader's row");
    await deliver(page, "arrival after the chat came back");
    await expectNotFollowed(page, before, "the arrival after the chat came back moved the reader's row");
  });

  // The ResizeObserver backstop: no panel button runs, the viewport crossing the
  // mobile breakpoint is what hides and shows the chat.
  test("a breakpoint hide and reveal keeps the reader's row through arrivals while hidden", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    // The room list open on mobile: chat hidden there, all panels on desktop.
    await hideChat(page, "hamburger-rooms-button");
    await page.setViewportSize({ width: 1280, height: 900 });
    await expect(chat(page), "premise: widening to desktop shows the chat").toBeVisible();
    await afterLayoutSettles(page);
    // A real scroll of the reader's at this width: the row saved at the mobile
    // width can sit at a gap the wider layout cannot reach yet, and only a
    // reader scroll replaces it.
    await readerScrollsWithoutGesture(page, await endMinus(page, 2 * PARKED_ABOVE_END_PX));
    await readerScrollsToEnd(page);
    await afterLayoutSettles(page);
    const before = await savedRow(page);

    await page.setViewportSize({ width: 390, height: 844 });
    await chatHidden(page);
    await deliverWhileHidden(page, 3);

    await page.setViewportSize({ width: 1280, height: 900 });
    await expect(chat(page)).toBeVisible();
    await afterLayoutSettles(page);
    await expectNotFollowed(page, before, "the breakpoint reveal showed the arrivals' end instead of the reader's row");
  });

  for (const reveal of [
    {
      by: "the back button",
      run: (page: Page) => page.getByTestId("rooms-back-button").click(),
    },
    {
      by: "widening to desktop",
      run: (page: Page) => page.setViewportSize({ width: 1280, height: 900 }),
    },
  ]) {
    test(`a room opened while hidden lands at its newest message and pages back, revealed by ${reveal.by}`, async ({
      page,
    }) => {
      await openRoomAtBottom(page, "Team Chat Room", DEEP_ROOM_PATH);
      await fillHistory(page);
      // The deep room must not inherit this offset.
      expect(await scrollTop(page), "premise: the old room sits well down its history").toBeGreaterThan(
        400,
      );

      await hideChat(page, "hamburger-rooms-button");
      await callRiverTest(page, "switchRoom", "Deep History Room");
      await expect
        .poll(() => renderedRowCount(page), {
          timeout: 5_000,
          message: "premise: the deep room should render while the chat is hidden",
        })
        .toBeGreaterThan(40);
      await afterLayoutSettles(page);

      await reveal.run(page);
      await expect(chat(page)).toBeVisible();
      await expect(page.getByRole("heading", { name: "Deep History Room" })).toBeVisible();
      await expectSettledAtBottom(page, "the room opened while hidden did not land at its newest message");

      // Paging back has to work with no arrival to unlock it.
      await afterLayoutSettles(page);
      const initialRows = await renderedRowCount(page);
      const head = await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        const head = c.querySelector('[data-testid="conversation-history"] > [data-item-key]');
        c.scrollTop = 0;
        return head!.getAttribute("data-item-key");
      });
      await expect
        .poll(() => renderedRowCount(page), {
          timeout: 5_000,
          message: "scrolling to the top did not load older history",
        })
        .toBeGreaterThan(initialRows + 40);
      const headNow = await page.evaluate(
        (key) =>
          Array.from(
            document.querySelectorAll('[data-testid="conversation-history"] > [data-item-key]'),
          ).findIndex((row) => row.getAttribute("data-item-key") === key),
        head,
      );
      expect(headNow, "the older rows should land above the old head").toBeGreaterThan(0);
    });
  }
});
