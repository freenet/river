import { test, expect, Page } from "@playwright/test";

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
// nothing: that path raises `force_scroll`, deliberately bypassing the pin.

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs.
const BOTTOM_THRESHOLD_PX = 100;
/// Slack for fractional layout after a scroll that did land at the bottom.
const AT_BOTTOM_EPSILON_PX = 4;

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
  await page.waitForFunction(() => (window as any).__riverTest !== undefined, {
    timeout: 30_000,
  });
}

async function selectRoom(page: Page, roomName: string) {
  await page.getByRole("button", { name: roomName }).click();
  await expect(page.getByRole("heading", { name: roomName })).toBeVisible({
    timeout: 5_000,
  });
  // The mobile projects keep their touch/UA emulation but run at the desktop
  // viewport these describes set, so the chat panel is always the visible one.
  // Asserted rather than assumed: if it were hidden, every geometry read below
  // would return 0 and the failures would point at scrolling rather than at
  // layout.
  await expect(page.locator("#chat-scroll-container")).toBeVisible({
    timeout: 5_000,
  });
}

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
  await page.evaluate((t) => (window as any).__riverTest.appendMessage(t), text);
  await expect(page.getByText(text, { exact: false }).last()).toBeVisible({
    timeout: 5_000,
  });
}

/// Deliver an inbound message one position from the end of the history: it
/// grows the content WITHOUT remounting the last row.
async function insertBeforeLast(page: Page, text: string) {
  await page.evaluate(
    (t) => (window as any).__riverTest.insertMessageBeforeLast(t),
    text
  );
}

/// Open a room and wait until the history has settled at its newest message.
async function openRoomAtBottom(page: Page, roomName: string) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, roomName);
  await expectSettledAtBottom(page, "opening a room should land on its newest message");
}

/// Simulate the reader dragging the history with a pointing device.
///
/// A synthetic `wheel` followed by a `scrollTop` assignment rather than
/// `page.mouse.wheel`, which is unsupported on mobile WebKit. Both halves
/// matter: the `wheel` is what tells the pin that the settle about to arrive is
/// the reader's, and the `scrollTop` write is what produces that settle.
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
/// scroll restoration produce none either. Nothing in the implementation
/// listens for gestures — whose settle it is, is decided by where the view came
/// to rest — so this is the ordinary path rather than a fallback, and these two
/// helpers should behave identically.
async function readerScrollsWithoutGesture(page: Page, top: number) {
  await page.evaluate((t) => {
    document.getElementById("chat-scroll-container")!.scrollTop = t;
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

/// Add enough history to have somewhere to scroll back through.
async function fillHistory(page: Page) {
  for (let i = 0; i < 8; i++) {
    await deliver(page, `filler ${i}: ${"y".repeat(200)}`);
  }
  await expectSettledAtBottom(page, "filler messages should have been followed");
}

test.describe("Conversation follows new messages (#486)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("keeps following a burst of arrivals after the composer takes the bottom off screen", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const roomyViewport = await viewportHeight(page);

    // Typing a long message grows the composer to its 168px maximum, which
    // takes more than the observer's 100px margin off the history in one step
    // — so the old gate latched with no network activity at all. This is why
    // #468 (the composer auto-resize) is a CAUSE of #486 rather than only a
    // performance cost.
    await page
      .getByTestId("message-input")
      .fill(Array.from({ length: 12 }, (_, i) => `draft line ${i}`).join("\n"));

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
    // that too, in both directions.
    //
    // It does NOT pin `reader_moved_up_since`'s `.min(max_scroll_top(...))`:
    // deleting that clamp was tried and this still passed, because the browser
    // fires a settle for its own clamping and the settle repairs the reference
    // before the next arrival. The `.min` narrows a race window rather than
    // deciding an outcome, and nothing here is strong enough to claim otherwise.
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
    await page.evaluate(() => {
      const rows = document.querySelectorAll("#chat-scroll-container .space-y-4 > *");
      (rows[rows.length - 1] as any).__riverProbe = "last-row";
    });

    await insertBeforeLast(
      page,
      "inserted above the last row: " + "x".repeat(400)
    );

    await expectSettledAtBottom(
      page,
      "content grew above the last row and the view did not follow it"
    );

    const lastRowSurvived = await page.evaluate(() => {
      const rows = document.querySelectorAll("#chat-scroll-container .space-y-4 > *");
      return (rows[rows.length - 1] as any).__riverProbe === "last-row";
    });
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

    // The reader scrolls back to the bottom THEMSELVES. This is the settle
    // handler's re-arm branch, and it is the only place in the suite that
    // reaches it: the scroll-to-latest button re-arms the pin directly, so a
    // suite that only used the button would still pass if the re-arm branch
    // were narrowed to, say, `distance <= 0`, and a reader who stopped a few
    // fractional pixels short would never be followed again.
    await readerScrollsWithoutGesture(page, await historyHeight(page));
    await expectSettledAtBottom(page, "the reader's own scroll should reach the bottom");
    await deliver(page, "arrived after the reader scrolled back down");
    await expectSettledAtBottom(
      page,
      "the reader returned to the bottom and the follow did not re-arm"
    );

    // The button is a second, independent way back, and it re-arms the pin
    // itself rather than through a settle.
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

    // No `wheel`, no `pointerdown`, no `touchstart`: only the settle handler
    // can notice this. Kept as its own test because an earlier implementation
    // DID lean on gesture events to decide whose settle a settle was, and this
    // is the shape that broke it — a native scrollbar drag on Firefox,
    // find-in-page, or focus-driven scrolling, none of which produce one.
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
