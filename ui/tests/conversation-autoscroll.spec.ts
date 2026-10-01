import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom, selectListedRoom } from "./example-room";

// Regression tests for freenet/river#486: new messages arrived and the view
// did not follow them.
//
// It looked intermittent. It is a permanent latch. Auto-scroll was gated on an
// IntersectionObserver watching a 1px sentinel with a 100px `rootMargin`, which
// answers "is the end of the history on screen right now?" — not "was the
// reader following the conversation". Once the gap passed 100px the gate read
// false and nothing re-armed it short of a manual scroll back down or a room
// switch. On the live Freenet room that meant 54 arrivals with the view frozen
// while the gap ratcheted from 147px to 2725px.
//
// Two things have to hold, and a suite that only checked the first would pass a
// "pin is always true" implementation that yanks the reader back down every
// time they try to read history:
//
//   1. anything that pushes the view off the bottom WITHOUT the reader asking
//      must not stop the view following new messages;
//   2. the reader scrolling away MUST stop it, and their coming back MUST start
//      it again.
//
// Which test carries which is worth stating, because the two groups fail for
// different reasons and only the first group fails against the unfixed code:
//
//   * `keeps following...`, `follows a mid-list insert...` and `keeps the
//     newest message in view when a resize reflows...` are the #486 tests. Each
//     fails against the pre-fix code at its own assertion.
//   * `respects the reader...`, `respects a reader scroll that produces no
//     gesture event` and `does not drag a parked reader down...` are the
//     opposite guard. They constrain the FIX, not the bug: the pre-fix code
//     also refuses to scroll a parked reader (for the wrong reason — its gate
//     has latched), so a revert makes them fail at their setup rather than at
//     the assertion that matters. Their teeth are against a wrong fix, and that
//     is established by mutating the fix, not by reverting it.
//
// Assumes the example-data build, which exposes `window.__riverTest` for
// delivering INBOUND messages. Sending through the composer would prove
// nothing: that path forces a snap to the bottom, deliberately bypassing the pin.

// Rendered history rows: display items plus date separators.
const HISTORY_ROWS = '[data-testid="conversation-history"] > *';

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
const BOTTOM_THRESHOLD_PX = 100;
/// Slack for fractional layout after a scroll that did land at the bottom.
const AT_BOTTOM_EPSILON_PX = 4;

/// A draft long enough to take more than BOTTOM_THRESHOLD_PX off the history.
const LONG_DRAFT = Array.from({ length: 12 }, (_, i) => `draft line ${i}`).join("\n");

/// scrollHeight - scrollTop - clientHeight: how far the end of the history is
/// below the visible area. 0 means the newest message is fully in view.
function distanceFromBottom(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    if (!el) return Number.NaN;
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  });
}

/// Total height of the rendered history, independent of where it is scrolled.
function historyHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.scrollHeight : Number.NaN;
  });
}

/// Height of the WINDOW onto the history. Shrinks when the composer grows.
function viewportHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.clientHeight : Number.NaN;
  });
}

function scrollTop(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.scrollTop : Number.NaN;
  });
}

async function expectSettledAtBottom(page: Page, why: string) {
  await expect
    .poll(() => distanceFromBottom(page), {
      timeout: 5_000,
      message: why,
    })
    .toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
}

/// Deliver an inbound message, exactly as an arriving network update does, and
/// wait until it is actually on the page.
///
/// Waiting on the text rather than on a fixed delay matters for the tests that
/// assert the view did NOT move: a timeout that expires before the render lands
/// passes whether or not the bug is present, and `retries: 2` would keep that
/// invisible.
async function deliver(page: Page, text: string) {
  await callRiverTest(page, "appendMessage", text);
  await expect(page.getByText(text, { exact: false }).last()).toBeVisible({
    timeout: 5_000,
  });
}

/// Open a room and wait until the history has settled at its newest message.
///
/// `path` lets a test opt into fixture variants the default build hides —
/// the windowed-history tests load `/?deep-history-room=1` to get a room
/// deeper than the render window without changing what every other spec sees.
async function openRoomAtBottom(page: Page, roomName: string, path = "/") {
  await page.goto(path);
  await waitForApp(page);
  await selectRoom(page, roomName);
  // Mobile projects run at the desktop viewport, so the chat panel is visible;
  // asserted since a hidden panel would make every geometry read below return 0.
  await expect(page.locator("#chat-scroll-container")).toBeVisible({ timeout: 5_000 });
  await expectSettledAtBottom(page, "opening a room should land on its newest message");
}

/// Simulate the reader dragging the history with a pointing device.
///
/// A synthetic `wheel` followed by a `scrollTop` assignment rather than
/// `page.mouse.wheel`, which is unsupported on mobile WebKit. Nothing listens for
/// gestures (the `scrollTop` write is the reader's scroll); the `wheel` is
/// harmless and keeps the helper realistic.
async function readerScrollsTo(page: Page, top: number) {
  await page.evaluate((t) => {
    const el = document.getElementById("chat-scroll-container")!;
    el.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -1 }));
    el.scrollTop = t;
  }, top);
}

/// The same, with NO gesture event at all.
///
/// Not a contrivance: a native scrollbar drag dispatches no pointer event to
/// the content on Firefox, and find-in-page, focus-driven scrolling and browser
/// scroll restoration produce none either. Unlike `readerScrollsTo`, this waits
/// for `scrollend`, so the scroll has landed before the test acts. The pending
/// scroll case is covered on purpose by the batched at-cap test, which does not
/// wait.
async function readerScrollsWithoutGesture(page: Page, top: number) {
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
async function expectStaysPut(page: Page, why: string) {
  const before = await scrollTop(page);
  await page.waitForTimeout(600);
  expect(await scrollTop(page), why).toBeCloseTo(before, 0);
}

/// Wait out the layout a test just provoked.
async function afterLayoutSettles(page: Page) {
  // Past the frame that carries the scroll event and the ResizeObserver.
  await page.evaluate(
    () =>
      new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve, 300))),
      ),
  );
}

/// The newest history message with any part inside the scroll container, and
/// how far its top sits above the container's bottom edge.
///
/// Found from `[id^="msg-"]`, not from the implementation's own row attribute,
/// so these tests stay a contract on behaviour.
function newestVisibleMessage(page: Page): Promise<{ id: string; gap: number } | null> {
  return page.evaluate(() => {
    const c = document.getElementById("chat-scroll-container");
    if (!c) return null;
    const box = c.getBoundingClientRect();
    let found: { id: string; gap: number } | null = null;
    for (const row of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
      const r = row.getBoundingClientRect();
      if (r.bottom > box.top && r.top < box.bottom) {
        found = { id: row.id, gap: box.bottom - r.top };
      }
    }
    return found;
  });
}

/// The geometry budget for "the reader's message did not move", in CSS px.
///
/// A test contract, deliberately NOT derived from the implementation's own
/// slack: it allows the residual understood so far (the scroll model reads
/// `scrollTop` and row gaps as whole pixels, and at a fractional device scale
/// rows sit at fractional offsets) and stays far below a visible row movement
/// (a fixture row is ~80-120px). If a production constant changes, investigate
/// measured drift rather than widening this.
const IN_PLACE_TOLERANCE_PX = 4;

type RowPosition = { id: string; gap: number };

/// The gap from the container's bottom edge to the top of the row whose DOM id
/// is `id`, or null when that row is not on the page.
function rowGap(page: Page, id: string): Promise<number | null> {
  return page.evaluate((rowId) => {
    const c = document.getElementById("chat-scroll-container");
    const row = document.getElementById(rowId);
    if (!c || !row || !c.contains(row)) return null;
    return c.getBoundingClientRect().bottom - row.getBoundingClientRect().top;
  }, id);
}

/// How far a remembered position has drifted, identity and gap read in ONE
/// evaluation. Infinity when the row is gone, or (with `newest`) when another
/// message is now the newest visible one: a missing row is a failure, never a
/// zero drift.
function positionDrift(page: Page, before: RowPosition, newest: boolean): Promise<number> {
  return page.evaluate(
    ({ id, gap, newest }) => {
      const c = document.getElementById("chat-scroll-container");
      if (!c) return Infinity;
      const box = c.getBoundingClientRect();
      if (newest) {
        let found: HTMLElement | null = null;
        for (const row of c.querySelectorAll<HTMLElement>('[id^="msg-"]')) {
          const r = row.getBoundingClientRect();
          if (r.bottom > box.top && r.top < box.bottom) found = row;
        }
        if (!found || found.id !== id) return Infinity;
        return Math.abs(box.bottom - found.getBoundingClientRect().top - gap);
      }
      const row = document.getElementById(id);
      if (!row || !c.contains(row)) return Infinity;
      return Math.abs(box.bottom - row.getBoundingClientRect().top - gap);
    },
    { ...before, newest },
  );
}

/// `before`'s row is back at its gap. `newest` (the default) also requires it to
/// be the newest visible message again; pass false for a known surviving row
/// that need not be (a fallback after the newest visible one was deleted).
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

/// `drift()` comes within `tolerance`, polled; with `hold`, then stays there for
/// five samples over 500ms (see `expectInPlace`).
async function expectDriftWithin(
  page: Page,
  drift: () => Promise<number>,
  why: string,
  { hold = false, tolerance = IN_PLACE_TOLERANCE_PX } = {},
) {
  await expect.poll(drift, { timeout: 5_000, message: why }).toBeLessThanOrEqual(tolerance);
  if (!hold) return;
  const drifts: number[] = [];
  for (let i = 0; i < 5; i++) {
    await page.waitForTimeout(100);
    drifts.push(await drift());
  }
  expect(
    Math.max(...drifts),
    `${why} (it converged, then drifted; samples every 100ms: ${drifts.map((d) => d.toFixed(2)).join(", ")})`,
  ).toBeLessThanOrEqual(tolerance);
}

/// The message `newestVisibleMessage` returned is the newest visible one again,
/// at the same gap. Keeps the 2px bound these older tests were written against.
async function expectSameMessageInPlace(page: Page, before: RowPosition, why: string) {
  await expectInPlace(page, before, why, { tolerance: 2 });
}

/// The fixture variant with rooms deeper than the render window.
const DEEP_ROOM_PATH = "/?deep-history-room=1";

/// Rendered history rows. A windowed tail is ~60 items plus a separator or two.
function renderedRowCount(page: Page): Promise<number> {
  return page.locator(HISTORY_ROWS).count();
}

/// Add enough history to have somewhere to scroll back through: `count` plain
/// messages (alternating authors, so one row each; no reactions or replies).
async function fillHistory(page: Page, count = 8) {
  for (let i = 0; i < count; i++) {
    await deliver(page, `filler ${i}: ${"y".repeat(200)}`);
  }
  await expectSettledAtBottom(page, "filler messages should have been followed");
}

/// Park a reader mid-history and remember the newest message they can see.
async function parkMidHistory(page: Page): Promise<RowPosition> {
  await openRoomAtBottom(page, "Team Chat Room");
  await fillHistory(page);
  await readerScrollsTo(page, Math.floor((await historyHeight(page)) / 2));
  await expect
    .poll(() => distanceFromBottom(page), {
      timeout: 5_000,
      message: "premise: the reader should be parked above the bottom",
    })
    .toBeGreaterThan(BOTTOM_THRESHOLD_PX);
  // The scroll has to have landed before we look at what is on screen.
  await afterLayoutSettles(page);
  const before = await newestVisibleMessage(page);
  expect(before, "premise: a message should be visible").not.toBeNull();
  return before!;
}

test.describe("Conversation follows new messages (#486)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("keeps following a burst of arrivals after the composer takes the bottom off screen", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const roomyViewport = await viewportHeight(page);

    // Typing a long message grows the composer, which takes more than the
    // observer's 100px margin off the history in one step — so the old gate
    // latched with no network activity at all. This is why #468 (the composer
    // auto-resize) is a CAUSE of #486 rather than only a performance cost.
    await page.getByTestId("message-input").fill(LONG_DRAFT);

    // The premise, asserted rather than assumed: the window over the history
    // has to shrink by more than the margin, or the latch would never have
    // armed and everything below would pass against the unfixed code too.
    //
    // It is the WINDOW that is measured, not the gap. Under the fix the gap
    // closes again immediately (the container is watched for resizes, so the
    // view follows the newest message down), which makes "the view is off the
    // bottom" unusable as a precondition — a complete fix erases it. What the
    // old gate keyed on, and what stays true either way, is that the container
    // lost more than 100px.
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message:
          "the composer did not grow enough to clear the observer's margin, so " +
          "this test is not exercising the latch",
      })
      .toBeLessThan(roomyViewport - BOTTOM_THRESHOLD_PX);

    await expectSettledAtBottom(
      page,
      "the composer grew over the newest message and the view did not follow it"
    );

    // The recorded failure: 54 arrivals, none of which scrolled, with the gap
    // ratcheting out to about three screens. Every one of these must land, and
    // the loop is what catches a fix that survives one arrival and then
    // re-latches.
    for (let i = 1; i <= 6; i++) {
      await deliver(page, `arrival ${i}`);
      await expectSettledAtBottom(
        page,
        `message ${i} arrived while a draft was open and the view did not follow it`
      );
    }

    // Clearing the draft gives the height back, so the container GROWS and the
    // browser clamps `scrollTop` down on its own. The follow has to survive
    // that too, in both directions. The clamp is layout's, not the reader's.
    await page.getByTestId("message-input").fill("");
    await expect
      .poll(() => viewportHeight(page), { timeout: 5_000 })
      .toBeGreaterThan(roomyViewport - BOTTOM_THRESHOLD_PX);
    await deliver(page, "arrived after the draft was cleared");
    await expectSettledAtBottom(
      page,
      "the draft was cleared and the next arrival was not followed"
    );
  });

  test("follows a mid-list insert that does not remount the last row", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");

    // Tag the last row so we can prove afterwards that it was diffed, not
    // remounted. Without this the test would pass for the wrong reason: a
    // remount would fire the old `onmounted` trigger, which is exactly the
    // path that already worked.
    //
    // It holds because of how the fixture ends, not by luck: "Team Chat Room"
    // finishes with messages from different authors, so its last group is a
    // single message whose key (its first message's id) cannot change when
    // something is inserted before it. `insertMessageBeforeLast` also signs
    // with its own key, so it can never merge into that group.
    await page.locator(HISTORY_ROWS).last().evaluate((row) => {
      (row as any).__riverProbe = "last-row";
    });

    await callRiverTest(
      page,
      "insertMessageBeforeLast",
      "inserted above the last row: " + "x".repeat(400)
    );

    await expectSettledAtBottom(
      page,
      "content grew above the last row and the view did not follow it"
    );

    const lastRowSurvived = await page
      .locator(HISTORY_ROWS)
      .last()
      .evaluate((row) => (row as any).__riverProbe === "last-row");
    expect(
      lastRowSurvived,
      "the last row remounted, so this exercised the old trigger rather than " +
        "the content-change trigger it is meant to pin"
    ).toBe(true);
  });

  test("respects the reader scrolling away, and re-arms when they come back", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    await readerScrollsTo(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await deliver(page, "arrived while reading history");
    await expectStaysPut(
      page,
      "a message arrived while the reader was scrolled up and yanked the view"
    );

    // The reader scrolls back to the bottom THEMSELVES. Only this reaches the
    // capture that re-pins them: the scroll-to-latest button re-pins directly, so
    // a suite that only used the button would pass if the threshold narrowed to,
    // say, `distance <= 0`, and a reader who stopped a few fractional pixels
    // short would never be followed again.
    await readerScrollsWithoutGesture(page, await historyHeight(page));
    await expectSettledAtBottom(page, "the reader's own scroll should reach the bottom");
    await deliver(page, "arrived after the reader scrolled back down");
    await expectSettledAtBottom(
      page,
      "the reader returned to the bottom and the follow did not re-arm"
    );

    // The button is a second, independent way back, and it re-pins directly
    // rather than through a captured scroll.
    await readerScrollsTo(page, 0);
    await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({
      timeout: 5_000,
    });
    await page.getByTestId("scroll-to-bottom").click();
    await expectSettledAtBottom(page, "the scroll-to-latest button should reach the bottom");
    await deliver(page, "arrived after the button was used");
    await expectSettledAtBottom(
      page,
      "the scroll-to-latest button must re-arm the follow"
    );
  });

  test("respects a reader scroll that produces no gesture event", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    // No `wheel`, no `pointerdown`, no `touchstart`: only the scroll event itself
    // can tell the reader moved. This is the shape a native scrollbar drag on
    // Firefox, find-in-page or focus-driven scrolling takes.
    await readerScrollsWithoutGesture(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await deliver(page, "arrived after a gesture-less scroll");
    await expectStaysPut(
      page,
      "the reader scrolled up without a gesture event and the view was yanked back"
    );
  });
});

test.describe("Conversation follows layout-only growth (#486)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("keeps the newest message in view when a resize reflows the history", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");

    const before = await historyHeight(page);

    // Narrowing the window makes every message wrap onto more lines, so the
    // history gets taller and its end drops below the fold. No state changed,
    // so the grouped-message memo does not re-run and the content-change
    // trigger never fires — only the ResizeObserver sees this. It is the same
    // class as a late-loading image or a font swapping in, which are the cases
    // a state-only trigger cannot cover.
    await page.setViewportSize({ width: 380, height: 900 });

    // The premise, asserted rather than assumed. How much a reflow grows the
    // history depends entirely on where the text happens to wrap: measured on
    // this fixture, 1280 -> 700 grows it by *zero* pixels, so a test written at
    // that width passes without the view ever having been pushed off the
    // bottom — it pins nothing. 1280 -> 380 grows it by ~340px. If a fixture or
    // layout change ever flattens that again, this fails loudly instead of
    // going quietly vacuous.
    await expect
      .poll(() => historyHeight(page), {
        timeout: 5_000,
        message:
          "narrowing the window did not make the history taller, so this test " +
          "is not exercising a reflow at all",
      })
      .toBeGreaterThan(before + BOTTOM_THRESHOLD_PX);

    await expectSettledAtBottom(
      page,
      "the history reflowed taller and the view did not follow it"
    );
  });

  // The reflow test above only trips on some font stacks (Linux CI's, not
  // macOS's): Chromium clamps scrollTop partway through the reflow, then the
  // history comes out taller. This makes the same clamp happen on every engine.
  test("follows a reflow that clamped the view on its way to a taller history @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await afterLayoutSettles(page);
    const { clamp, laidOutAgain } = await page.evaluate(() => {
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
      return { clamp: before - clamped, laidOutAgain };
    });
    expect(clamp, "premise: hiding the newest row must clamp the view").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX
    );
    expect(
      laidOutAgain,
      "premise: the rewrap must change the history's height or the width it wraps at"
    ).toBe(true);
    await expectSettledAtBottom(page, "the reflow's own clamp was read as the reader scrolling up");
  });

  test("follows history that grows in the same frame the composer collapses", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const roomyViewport = await viewportHeight(page);
    await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      for (const type of ["scroll", "scrollend"]) {
        c.addEventListener(type, () => {
          (window as any).__riverLastScrollEvent = type;
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
      .toBeLessThan(roomyViewport - BOTTOM_THRESHOLD_PX);
    await expectSettledAtBottom(page, "the composer grew and the view did not follow it");
    // A scroll event still pending from the follow would capture after the clamp
    // and hide the race.
    await page.waitForFunction(
      () => (window as any).__riverLastScrollEvent === "scrollend",
      undefined,
      { timeout: 5_000 },
    );

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
          (window as any).__riverSameFrame = {
            collapsed,
            grew: c.scrollHeight - before,
          };
        };
        document.addEventListener("input", onInput, { once: true });
      },
      { grow: GROWTH_PX, collapsedAbove: roomyViewport - BOTTOM_THRESHOLD_PX },
    );
    await page.getByTestId("message-input").fill("");

    const premise = await page.evaluate(() => (window as any).__riverSameFrame);
    expect(
      premise.collapsed,
      "premise: the composer should collapse inside the input handler",
    ).toBe(true);
    expect(premise.grew, "premise: the newest row should have grown").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );
    await expectSettledAtBottom(
      page,
      "the history grew in the same frame the composer collapsed and the view did not follow it",
    );
  });

  test("does not drag a parked reader down when a resize reflows the history", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    await readerScrollsTo(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    // Same reflow as above, with the reader parked. The follow and the refusal
    // to follow run through the same ResizeObserver, so this is the half that
    // stops "keep the newest message in view" from becoming "never let the
    // reader look away".
    await page.setViewportSize({ width: 380, height: 900 });
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);
  });
});

// The reader's position is a message, not an offset: whichever message is the
// newest one on screen stays where it was when the window changes shape.
test.describe("The newest visible message stays in view", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("the same message is still in place after resizing there and back @fractional-geometry", async ({ page }) => {
    const before = await parkMidHistory(page);
    await page.setViewportSize({ width: 380, height: 900 });
    await expectSameMessageInPlace(
      page,
      before,
      "the resize moved the message the reader was looking at",
    );
    await page.setViewportSize({ width: 1280, height: 900 });
    await expectSameMessageInPlace(
      page,
      before,
      "resizing back did not return the reader's message to where it was",
    );
  });

  // The reader's position is measured from the container's BOTTOM edge, and the
  // composer takes height off that edge. So a growing draft moves a parked
  // reader's text up with it rather than covering it, and clearing the draft
  // moves it back. Intentional: the newest line they could see stays in sight.
  test("a parked reader's message keeps its gap from the bottom as the composer grows and clears @fractional-geometry", async ({
    page,
  }) => {
    const before = await parkMidHistory(page);
    const roomy = await viewportHeight(page);

    await page.getByTestId("message-input").fill(LONG_DRAFT);
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message: "premise: the draft should take more than the follow band off the history",
      })
      .toBeLessThan(roomy - BOTTOM_THRESHOLD_PX);
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

  test("stops following when the reader scrolls up after a resize", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await page.setViewportSize({ width: 380, height: 900 });
    await expectSettledAtBottom(page, "the resize should have kept the view at the bottom");

    await readerScrollsTo(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    await deliver(page, "arrival after the reader left");
    await expectStaysPut(page, "an arrival dragged a reader who scrolled up after a resize");
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
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await deliver(page, "arrival after a same-frame scroll");
    await expectStaysPut(page, "an arrival dragged a reader whose scroll shared a frame with a rewrap");
  });
});

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

/// The rows `fillHistory` delivered, in order, by their `filler N:` text.
function fillerRows(rows: AnchorRow[]): AnchorRow[] {
  return rows.filter((row) => row.id.startsWith("msg-") && /^filler \d+:/.test(row.text));
}

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
  await readerScrollsTo(page, target);
  await expect
    .poll(async () => Math.abs((await scrollTop(page)) - target), {
      timeout: 5_000,
      message: "premise: the reader's scroll should land where it was aimed",
    })
    .toBeLessThanOrEqual(1);
  await afterLayoutSettles(page);
  expect(
    await distanceFromBottom(page),
    "premise: the reader should be parked outside the follow band",
  ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
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

  test("a deleted newest visible message falls back to the row above it @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, 24);
    const deleted = fillerRows(await anchorRows(page))[11];
    await parkOnRow(page, deleted.id);

    const rows = await anchorRows(page);
    const at = rows.findIndex((row) => row.id === deleted.id);
    const survivor = rows[at - 1];
    // A date separator or event row here would be the first fallback instead,
    // and the premise below would not be the one the test claims.
    expect(
      survivor.id,
      "premise: the anchor-bearing row right above the reader's message is a message",
    ).toMatch(/^msg-/);
    const farAbove = fillerRows(rows)[1];
    const viewport = await viewportHeight(page);
    expect(farAbove.bottom, "premise: the second deletion is well above the viewport").toBeLessThan(
      -viewport / 2,
    );

    const before = { id: survivor.id, gap: (await rowGap(page, survivor.id))! };
    const heightBefore = await historyHeight(page);
    await removeMessages(page, [deleted.id, farAbove.id]);
    // Unanswered, the row removed above would move the survivor by its height.
    expect(
      heightBefore - (await historyHeight(page)),
      "premise: the deletion should change the geometry materially",
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await expectInPlace(
      page,
      before,
      "the reader's message was deleted and the row above it did not keep its place",
      { newest: false, hold: true },
    );
    await deliver(page, "arrival after the reader's message was deleted");
    await expectInPlace(page, before, "an arrival after the deletion moved the reader", {
      newest: false,
      hold: true,
    });
  });

  test("the reader stays at their offset when the whole remembered neighbourhood is deleted @fractional-geometry", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page, 24);
    await parkOnRow(page, fillerRows(await anchorRows(page))[11].id);

    // Every row on screen, and every row within a viewport's height above it.
    // The model remembers a handful of rows ending at the newest visible one;
    // this covers that neighbourhood whatever the handful is, without mirroring
    // its size.
    const viewport = await viewportHeight(page);
    const rows = await anchorRows(page);
    const newestAt = rows.findLastIndex((row) => row.top < viewport && row.bottom > 0);
    const firstAt = rows.findIndex((row) => row.bottom > -viewport);
    const doomed = rows.slice(firstAt, newestAt + 1);
    expect(
      doomed.every((row) => row.id.startsWith("msg-") && /^filler \d+:/.test(row.text)),
      "premise: the deleted neighbourhood holds plain messages only, no separator or event row",
    ).toBe(true);
    expect(rows[firstAt].top, "premise: the deletion reaches a viewport above the top edge").toBeLessThanOrEqual(
      -viewport,
    );
    expect(firstAt, "premise: rows remain above the deletion").toBeGreaterThan(0);

    const top = await scrollTop(page);
    await removeMessages(page, doomed.map((row) => row.id));
    expect(
      (await anchorRows(page)).filter((row) => doomed.some((d) => d.id === row.id)),
      "premise: no anchor-bearing row of the neighbourhood survives",
    ).toEqual([]);
    // The old offset must still be reachable with room to spare, so a clamp
    // cannot be what keeps (or moves) the view.
    const after = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      return { max: c.scrollHeight - c.clientHeight, viewport: c.clientHeight };
    });
    expect(after.viewport, "premise: the history is laid out").toBeGreaterThan(0);
    expect(
      after.max - top,
      "premise: the old offset stays reachable, well outside the follow band",
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await expectDriftWithin(
      page,
      () => offsetDrift(page, top),
      "with no remembered row left, the reader's offset was moved",
      { hold: true },
    );
    await deliver(page, "arrival after the neighbourhood was deleted");
    await expectDriftWithin(
      page,
      () => offsetDrift(page, top),
      "an arrival after the neighbourhood was deleted moved the reader",
      { hold: true },
    );
    expect(await distanceFromBottom(page), "the arrival repinned the reader").toBeGreaterThan(
      BOTTOM_THRESHOLD_PX,
    );
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
      BOTTOM_THRESHOLD_PX,
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
        .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

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

// freenet/river#723: the reader scrolls back down to the newest message, and an
// arrival lands BEFORE that scroll's event is delivered. If the arrival's
// restore went on the stale "parked" state it would hold the view where the
// reader just left, and their scroll to the end would be lost.
test.describe("An arrival ahead of the reader's scroll event (#723)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a reader who scrolled to the end follows a tall arrival that beat their scroll event", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await afterLayoutSettles(page);
    expect(
      await distanceFromBottom(page),
      "premise: the reader is parked well above the end",
    ).toBeGreaterThan(4 * BOTTOM_THRESHOLD_PX);

    const tall = `tall arrival ahead of the scroll event\n${Array.from({ length: 12 }, (_, i) => `line ${i}`).join("\n")}`;
    // One sequence, in a frame callback: the scroll's event is then due in the
    // NEXT frame, while the hook's delivery runs on a timer before it.
    const race = await page.evaluate(
      (text) =>
        new Promise<{ order: string[]; arrivalHeight: number }>((resolve) => {
          const c = document.getElementById("chat-scroll-container")!;
          const history = document.querySelector('[data-testid="conversation-history"]')!;
          const order: string[] = [];
          let arrivalHeight = 0;
          const marker = "tall arrival ahead of the scroll event";
          const patched = new MutationObserver(() => {
            const row = Array.from(history.querySelectorAll<HTMLElement>('[id^="msg-"]')).find((r) =>
              r.textContent?.includes(marker),
            );
            if (row && !order.includes("patch")) {
              order.push("patch");
              arrivalHeight = row.getBoundingClientRect().height;
            }
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
            patched.disconnect();
            c.removeEventListener("scroll", onScroll);
            resolve({ order, arrivalHeight });
          }
          patched.observe(history, { childList: true, subtree: true });
          c.addEventListener("scroll", onScroll);
          requestAnimationFrame(() => {
            c.scrollTop = c.scrollHeight;
            (window as any).__riverTest.appendMessage(text);
          });
        }),
      tall,
    );
    expect(
      race.order,
      `premise: the arrival's patch must land before the scroll event (observed: ${race.order.join(" → ") || "nothing"})`,
    ).toEqual(["patch", "scroll"]);
    expect(
      race.arrivalHeight,
      "premise: the arrival is taller than the follow band",
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await expectSettledAtBottom(
      page,
      `the reader scrolled to the end and the arrival ahead of their scroll event was not followed (order: ${race.order.join(" → ")})`,
    );
    await deliver(page, "arrival after the race");
    await expectSettledAtBottom(page, "the follow did not survive the race");
  });
});

// Regression tests for freenet/river#501: the #498 windowed tail slid its
// start index forward on every arrival, removing the oldest rendered rows in
// the same patch that appended the new message. Browser scroll anchoring
// rewrote scrollTop to hold the visible content still, the old code read that
// as the reader moving up, and both follow paths stood down — so a room deeper
// than the render window stopped following arrivals entirely, while every room
// the old suite seeded (~15-20 items vs a 60-item window) kept passing on the
// pre-window code path.
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
test.describe("Windowed history follows arrivals (#501)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  const DEEP_ROOM = "Deep History Room";
  /// Seeded exactly at its max_recent_messages cap: every delivered arrival
  /// drains the oldest message, shifting every item index (#505 blocker 1).
  const CAPPED_ROOM = "Capped History Room";

  /// The premise all three tests stand on: the windowed render path is
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

  test("keeps following a burst of arrivals in a windowed room", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // The recorded #501 failure mode: with the reader pinned to the bottom,
    // each arrival slid the window, the shifted scrollTop read as reader
    // movement, and the view never followed again. The loop is what catches a
    // fix that survives one arrival and then latches. Delivered arrivals
    // alternate authors (see `test_author` in test_hooks.rs), so each one
    // is its own display item and the loop interleaves window GROWTH with the
    // bottom TRIM — six arrivals folding into one group would
    // exercise the windowing arithmetic zero times.
    for (let i = 1; i <= 6; i++) {
      await deliver(page, `windowed arrival ${i}`);
      await expectSettledAtBottom(
        page,
        `arrival ${i} in a windowed room was not followed`
      );
    }

    // Following six arrivals must not have cost the window its bound: each
    // follow snap ends at the bottom, and the capture there trims the window
    // back toward its initial size. Polled because the trim lands
    // asynchronously.
    //
    // NOTE on what this test does and does not guard. It is what CAUGHT the
    // trim/re-anchor bug (a trim shrinks the content, the browser clamps
    // scrollTop, and an arrival landing before that clamp was not followed) —
    // but it only failed 5 runs in 16, so under the suite's `retries: 2` a
    // regression has roughly a 3% chance of failing CI hard. The timing is not
    // practical to force deterministically from a browser test. The
    // DETERMINISTIC guard is the source pin `the_trim_stays_gated_at_the_bottom`
    // in conversation.rs; do not assume this test covers a revert.
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message:
          "the bottom trim should return the window to ~initial size " +
          "after a followed burst",
      })
      .toBeLessThan(67);
  });

  test("arrivals do not move a reader parked in a windowed room's history", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // Park mid-history: far enough from the bottom to unpin, far enough from
    // the top not to trigger a backfill.
    const mid = Math.floor((await historyHeight(page)) / 2);
    await readerScrollsTo(page, mid);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

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
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

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
    // group's key is its first message's id — the new key exists in no
    // pre-patch row). The window must re-anchor on the surviving neighbors
    // (spare keys) and the restore must find a surviving anchor row, or the
    // parked reader's view is torn away (#505 re-review blocker).
    //
    // No scrollTop-stability assertion here, deliberately: the compensation
    // MOVES scrollTop to hold the CONTENT still. The probed row's rect is
    // the thing that must not move.
    await openRoomAtBottom(page, CAPPED_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // Park just far enough up to be unpinned, NOT mid-history: the 61-message
    // batch drains ~30 display items off the FRONT of a 74-item room, so a
    // row tagged mid-history is inside the pruned range and legitimately
    // leaves the DOM — which row exactly depends on per-engine row heights,
    // so tagging there is flaky by construction rather than by timing. The
    // rows just above the fold are the newest ones; they survive the drain,
    // and holding THEM still is the property under test.
    //
    // Deliberately NO wait before delivering: the batch lands while the
    // reader's scroll event may still be pending, so this also covers the pin
    // being stale-true. `restore` must take that scroll in first, and the test
    // would go quiet about it if it waited the event out.
    const parkedAt = Math.max(
      0,
      (await historyHeight(page)) - (await viewportHeight(page)) - 400
    );
    await readerScrollsTo(page, parkedAt);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

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

    // Park mid-history so the batch below grows the window (a pinned reader's
    // bottom trim would remove the divergence before paging).
    const mid = Math.floor((await historyHeight(page)) / 2);
    await readerScrollsTo(page, mid);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);

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

  test("opening a deep room lands settled at the bottom with the initial window", async ({
    page,
  }) => {
    // `openRoomAtBottom` itself asserts the settle; the premise check is what
    // rules out the H2 failure shape, where the backfill sentinel fires from
    // scrollTop 0 before the opening snap and cascades the window over the
    // whole room (the row count would be ~200, not ~62). Note the H2 race is
    // timing-dependent in a live browser — this test catches it when it
    // fires, but the deterministic guard is the source pin on the sentinel's
    // `opening_snap_done` mount gate in conversation.rs.
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);

    // And it STAYS settled: a late backfill restore racing the opening snap
    // (H3) would park the view at the restore anchor moments later.
    await expectStaysPut(page, "the view moved after the room-open snap settled");
    await expectSettledAtBottom(
      page,
      "the room should still be at its newest message after settling"
    );
  });
});

// On mobile the chat column is `display:none` while the room list or the member
// list is open. Every geometry read is then 0, and none of it is where the
// reader is. The browser keeps `scrollTop` across the hide, so a plain round
// trip comes back in place on its own; these change something while hidden.
test.describe("The hidden mobile chat column", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  const chat = (page: Page) => page.locator("#chat-scroll-container");

  /// Open another mobile panel and wait out the hide's own observer pass.
  async function hideChat(page: Page, opener: "hamburger-rooms-button" | "header-members-button") {
    await page.getByTestId(opener).filter({ visible: true }).click();
    await expect(chat(page)).toBeHidden({ timeout: 5_000 });
    await expect
      .poll(() => viewportHeight(page), { message: "premise: the hidden chat has no height" })
      .toBe(0);
    await afterLayoutSettles(page);
  }

  test("a parked reader keeps their message when rows above it change while hidden @fractional-geometry", async ({
    page,
  }) => {
    // At its cap, so every arrival drains the oldest message above the reader.
    await openRoomAtBottom(page, "Capped History Room", DEEP_ROOM_PATH);
    await readerScrollsTo(page, (await historyHeight(page)) - (await viewportHeight(page)) - 400);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(BOTTOM_THRESHOLD_PX);
    await afterLayoutSettles(page);
    const before = await newestVisibleMessage(page);
    expect(before, "premise: a message should be visible").not.toBeNull();

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

    await page.getByTestId("members-back-button").click();
    await expect(chat(page)).toBeVisible();
    await afterLayoutSettles(page);
    await expectSameMessageInPlace(
      page,
      before!,
      "the reader came back to a different place after the rows above them changed",
    );
  });

  test("a following reader still follows after arrivals while hidden", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    expect(await scrollTop(page), "premise: the bottom is well down the history").toBeGreaterThan(400);

    await hideChat(page, "hamburger-rooms-button");
    for (let i = 1; i <= 3; i++) {
      const text = `hidden arrival ${i}: ${"z".repeat(200)}`;
      await callRiverTest(page, "appendMessage", text);
      await expect(page.getByText(text)).toBeAttached({ timeout: 5_000 });
    }
    await afterLayoutSettles(page);

    await selectListedRoom(page, "Team Chat Room");
    await expectSettledAtBottom(page, "arrivals while the chat was hidden were not followed");
    await deliver(page, "arrival after the chat came back");
    await expectSettledAtBottom(page, "the follow did not survive the chat being hidden");
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
