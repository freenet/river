import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import {
  ALL_PROJECTS,
  AT_BOTTOM_EPSILON_PX,
  BOTTOM_THRESHOLD_PX,
  HISTORY_ROWS,
  deliver,
  deliverOffscreen,
  distanceFromBottom,
  expectParkedAwayFromEnd,
  expectRowHeld,
  expectSettleWithheld,
  expectSettledAtBottom,
  expectStaysPut,
  fillHistory,
  historyHeight,
  holdSettleEvents,
  knownFailure,
  nextFrames,
  openRoomAtBottom,
  readerScrollsTo,
  readerScrollsWithoutGesture,
  readingRow,
  releaseSettleEvents,
  scrollTop,
  viewportHeight,
  withheld,
} from "./history-geometry";

/// A draft long enough to take more than BOTTOM_THRESHOLD_PX off the history.
const LONG_DRAFT = Array.from({ length: 12 }, (_, i) => `draft line ${i}`).join("\n");

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
    // A follow still settling would re-record the mark after the clamp and
    // hide the race.
    await page.waitForFunction(
      () => (window as any).__riverLastScrollEvent === "scrollend",
      undefined,
      { timeout: 5_000 },
    );

    // On `document`, so it runs after the app's own input handler has
    // collapsed the composer, before any frame.
    const GROWTH_PX = 80;
    // Under BOTTOM_THRESHOLD_PX: the pin stays armed, so only
    // reader_moved_up_since can refuse.
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

// Regression tests for freenet/river#501: the #498 windowed tail slid its
// start index forward on every arrival, removing the oldest rendered rows in
// the same patch that appended the new message. Browser scroll anchoring
// rewrote scrollTop to hold the visible content still, `reader_moved_up_since`
// attributed the browser's adjustment to the reader, and both follow paths
// stood down — so a room deeper than the render window stopped following
// arrivals entirely, while every room the old suite seeded (~15-20 items vs a
// 60-item window) kept passing on the pre-window code path.
//
// Every test here therefore asserts its PREMISE first — the backfill sentinel
// is attached and the rendered row count is a windowed tail, not the whole
// fixture — so a fixture change that shrinks the room below the window makes
// these fail loudly instead of quietly regressing into small-room tests.
//
// The fixture: `?deep-history-room=1` adds a room of 188 alternating-author
// messages (alternation makes messages == display items, so >60 items is a
// guarantee) plus the 13 standard fixture messages, 201 in all. The default
// fixture is untouched; the describes above still exercise the small-room path.
test.describe("Windowed history follows arrivals (#501)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  const DEEP_ROOM = "Deep History Room";
  /// Seeded exactly at its max_recent_messages cap: every delivered arrival
  /// drains the oldest message, shifting every item index (#505 blocker 1).
  const CAPPED_ROOM = "Capped History Room";
  const DEEP_ROOM_PATH = "/?deep-history-room=1";

  /// A windowed tail is ~60 items + a separator or two; the whole fixture is
  /// ~200 items.
  function renderedRowCount(page: Page): Promise<number> {
    return page.locator(HISTORY_ROWS).count();
  }

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
    // settle-at-bottom TRIM — six arrivals folding into one group would
    // exercise the windowing arithmetic zero times.
    for (let i = 1; i <= 6; i++) {
      await deliver(page, `windowed arrival ${i}`);
      await expectSettledAtBottom(
        page,
        `arrival ${i} in a windowed room was not followed`
      );
    }

    // Following six arrivals must not have cost the window its bound: each
    // follow snap settles at the bottom, and that settle trims the window
    // back toward its initial size. Polled because the last settle's trim
    // lands asynchronously.
    //
    // NOTE on what this test does and does not guard. It is what CAUGHT the
    // trim/re-anchor bug (a trim shrinks the content, the browser clamps
    // scrollTop, and an arrival landing before that clamp's settle was not
    // followed) — but it only failed 5 runs in 16, so under the suite's
    // `retries: 2` a regression has roughly a 3% chance of failing CI hard.
    // The timing is not practical to force deterministically from a browser
    // test. The DETERMINISTIC guard is the source pin
    // `the_window_trims_at_the_bottom_and_backfill_defers_to_the_pin` in
    // conversation.rs, which pins the flag, the guard and the scroll call;
    // do not assume this test covers a revert.
    await expect
      .poll(() => renderedRowCount(page), {
        timeout: 5_000,
        message:
          "the settle-at-bottom trim should return the window to ~initial size " +
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
    // (spare keys) and the reposition must measure through a surviving row
    // (probe walk), or the parked reader's view is torn away (#505
    // re-review blocker).
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
    // Deliberately NO settle wait before delivering: the batch lands while
    // the reader's `scrollend` is still in flight, so this also covers the
    // pre-settle window, where `pinned_to_bottom` is stale-true and only
    // `reader_moved_up_since` stands the follow paths down. That is exactly
    // what the relative `last_scroll_top` update protects, so the test would
    // go quiet about it if it waited the pin out first.
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
    // Let scroll events from the reposition settle before measuring.
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
    // follow-snap settle would trim the divergence away before paging).
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

// Scroll regression coverage. Cases A01 and A04–A06 live below: settle timing
// (A01), the render ceiling and uneven-row trim (A04, A05), and own-send snap
// (A06). The others are in other specifications: content above a parked reader
// (A02) and a hidden panel (A03) in conversation-history-position.spec.ts,
// unseen arrivals (A07) in room-unread-badge.spec.ts, and edit-form
// reachability (A08) in message-layout.spec.ts. Tests marked CURRENT POLICY
// characterize behavior that may be changed on purpose; the rest are
// invariants.

// Fixture assumptions from ui/src/example_data.rs, named where they are consumed.
/// Rows the history renders when a room opens, and trims back to.
const INITIAL_RENDERED_ITEMS = 60;
/// Rows a parked reader's range grows to before its end is held (WINDOW_ITEMS_CEILING).
const RENDER_CEILING_ITEMS = INITIAL_RENDERED_ITEMS * 4;
/// How far above the end the newer-history trigger reaches (BACKFILL_LEAD_PX).
const NEWER_TRIGGER_REACH_PX = 800;
/// Tall rows at the head of "Tall Head Room" (at its message cap).
const TALL_HEAD_TALL_ROWS = 8;
/// Tall rows at the head of "Uneven Tail Room", numbered 00 up.
const UNEVEN_TAIL_TALL_ROWS = 40;

test.describe("Arrival before the reader's settle (A01)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // freenet/river#723. The pin re-arms only when the reader's settle lands, so
  // an arrival patched between "back at the bottom" and that settle is not
  // followed. CURRENT POLICY: main intends to follow here. Dropping
  // arrival-following would keep only "no unrequested navigation" and change
  // this test.
  test("an arrival patched before the reader's return-to-bottom settles is followed", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page);

    await holdSettleEvents(page);
    await page.evaluate(() => {
      const el = document.getElementById("chat-scroll-container")!;
      el.scrollTop = el.scrollHeight;
    });
    await expectSettleWithheld(page);
    expect(await distanceFromBottom(page), "premise: the reader is back at the end").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    await deliverOffscreen(page, "arrived before the reader's settle");
    await nextFrames(page);
    await releaseSettleEvents(page);

    knownFailure(ALL_PROJECTS, "freenet/river#723");
    await expectSettledAtBottom(
      page,
      "the reader returned to the end, a message landed before their settle, and it was not followed",
      2_000,
    );
  });

  // freenet/river#508. The reader scrolls up and, before their settle, a burst
  // drains TALL rows from the head of an at-cap room, so the history shrinks
  // under them and the browser clamps their offset. With the pin still
  // stale-true that clamp reads as "not moved", and the view is taken to the
  // end. Invariant: the row they scrolled to stays where it is.
  test("a shrinking at-cap burst before the reader's settle keeps their row", async ({ page }) => {
    await openRoomAtBottom(page, "Tall Head Room", "/?uneven-history=1");
    await expect(
      page.locator("[data-item-key]").first(),
      "premise: the room's oldest rows are the tall ones",
    ).toContainText("tall filler");

    await holdSettleEvents(page);
    await readerScrollsTo(page, (await scrollTop(page)) - 300);
    await expectSettleWithheld(page);
    const row = await readingRow(page);
    expect(row, "premise: a row is fully in view after scrolling up").not.toBeNull();
    const heightBefore = await historyHeight(page);

    // One arrival per tall row: the at-cap prune drains exactly those.
    await callRiverTest(page, "appendMessages", TALL_HEAD_TALL_ROWS);
    await expect(page.getByText("tall filler", { exact: false })).toHaveCount(0, { timeout: 5_000 });
    expect(
      heightBefore - (await historyHeight(page)),
      "premise: the burst should shrink the history by more than the reader scrolled",
    ).toBeGreaterThan(300);
    await nextFrames(page);
    await releaseSettleEvents(page);

    knownFailure(ALL_PROJECTS, "freenet/river#508");
    await expectRowHeld(page, row!.key, row!.top, "the reader's row moved when a burst shrank the history before their settle");
  });
});

/// Scroll the history to its top and wait for backfill to add rows.
async function backfillOnce(page: Page, why: string) {
  const before = await page.locator("[data-item-key]").count();
  await page.evaluate(() => {
    document.getElementById("chat-scroll-container")!.scrollTop = 0;
  });
  await expect.poll(() => page.locator("[data-item-key]").count(), { message: why }).toBeGreaterThan(before);
}

/// Backfill until the tall rows render, then return to the end. Returns the
/// rendered row counts the history went through after the return.
async function backfillThenReturn(page: Page): Promise<number[]> {
  const tallest = page.getByText(`tall filler ${UNEVEN_TAIL_TALL_ROWS - 1} line 00`, { exact: false });
  for (let i = 0; i < 5 && (await tallest.count()) === 0; i++) {
    await backfillOnce(page, "backfill revealed no more rows on the way to the tall rows");
  }
  await expect(tallest, "premise: backfill should reach the tall rows").toHaveCount(1);
  return observeRowCounts(page, { returnToEnd: true });
}

/// The rendered row counts the history goes through over two seconds, each
/// recorded only when it changes. With `returnToEnd`, the jump to the end is
/// made after the observer is installed, so its first patch is not missed.
function observeRowCounts(page: Page, { returnToEnd = false } = {}): Promise<number[]> {
  return page.evaluate(async (returnToEnd) => {
    const hist = document.querySelector('[data-testid="conversation-history"]')!;
    const counts: number[] = [];
    let last = hist.querySelectorAll("[data-item-key]").length;
    const observer = new MutationObserver(() => {
      const n = hist.querySelectorAll("[data-item-key]").length;
      if (n !== last) counts.push((last = n));
    });
    observer.observe(hist, { childList: true });
    try {
      if (returnToEnd) {
        const c = document.getElementById("chat-scroll-container")!;
        c.scrollTop = c.scrollHeight;
      }
      await new Promise((r) => setTimeout(r, 2_000));
    } finally {
      observer.disconnect();
    }
    return counts;
  }, returnToEnd);
}

test.describe("Render ceiling and trimming (A04, A05)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  /// Park in the Deep History Room, then send a 200-message burst that passes
  /// the ceiling and prunes the oldest messages, but not the reader's.
  async function parkPastTheCeiling(page: Page) {
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1");
    // Below the backfill strip (top 800px), near the head of the window.
    await readerScrollsWithoutGesture(page, 1_000);
    const row = await readingRow(page, "history filler");
    expect(row, "premise: a filler row is fully in view").not.toBeNull();
    const fillerIndex = Number(/history filler (\d+)/.exec(row!.text)![1]);

    const DEEP_ROOM_SEEDED = 201; // 188 fillers + 13 standard messages
    const DEEP_ROOM_CAP = 300; // DEEP_ROOM_MAX_RECENT_MESSAGES
    const BURST = 200;
    const pruned = DEEP_ROOM_SEEDED + BURST - DEEP_ROOM_CAP;
    expect(
      fillerIndex,
      "premise: the reader's message survives the prune, so it is still in the room",
    ).toBeGreaterThanOrEqual(pruned);
    await callRiverTest(page, "appendMessages", BURST);
    // The newest arrivals are withheld, so check the oldest one instead.
    await expect(page.getByText("batched arrival 00", { exact: false })).toHaveCount(1, {
      timeout: 10_000,
    });
    await expect.poll(() => withheld(page), { message: "premise: the burst should pass the ceiling" }).toBeGreaterThan(0);
    await expect(page.getByTestId("scroll-to-bottom"), "premise: the burst should have landed").toBeVisible();
    await expect(
      page.getByText(`history filler ${String(pruned - 1).padStart(2, "0")}:`, { exact: false }),
      "premise: the prune should have landed",
    ).toHaveCount(0);
    return row!;
  }

  // Past the ceiling a parked reader's range holds: the row stays, later
  // arrivals are withheld, and jump to latest still works.
  test("repeated bursts past the ceiling keep the reader put and the DOM bounded", async ({ page }) => {
    const row = await parkPastTheCeiling(page);
    await expectRowHeld(page, row.key, row.top, "a burst past the render ceiling took the reader off a message the room still holds");
    const rows = await page.locator("[data-item-key]").count();
    const before = await withheld(page);

    // Small enough that its prune stays above the rendered range.
    await callRiverTest(page, "appendMessages", 20);
    await expect.poll(() => withheld(page), { message: "premise: the second burst should land" }).toBeGreaterThan(before);
    await callRiverTest(page, "appendMessage", "newest after the bursts");
    await expect.poll(() => withheld(page), { message: "premise: the last arrival should land" }).toBeGreaterThan(before + 20);

    expect(await page.locator("[data-item-key]").count(), "arrivals below a held range grew the DOM").toBe(rows);
    await expectRowHeld(page, row.key, row.top, "a burst below a held range moved the reader's row");

    await page.getByTestId("scroll-to-bottom").click();
    await expect(page.locator("[data-item-key]").last(), "jump to latest did not reach the newest message").toContainText(
      "newest after the bursts",
    );
    await expectSettledAtBottom(page, "jump to latest did not land at the newest message");
  });

  // Reading down a held range pages withheld messages in until it reaches the
  // newest one.
  test("reading down a held range pages the withheld messages in", async ({ page }) => {
    await parkPastTheCeiling(page);
    await callRiverTest(page, "appendMessage", "newest after the burst");
    await expect.poll(() => withheld(page)).toBeGreaterThan(0);

    // Already at the end means no scroll and no settle to wait for.
    const readToEnd = async () => {
      if ((await distanceFromBottom(page)) > AT_BOTTOM_EPSILON_PX) {
        await readerScrollsWithoutGesture(page, await historyHeight(page));
      }
    };
    for (let i = 0; i < 10 && (await withheld(page)) > 0; i++) {
      const before = await withheld(page);
      await readToEnd();
      await expect.poll(() => withheld(page), { message: "reading down revealed no newer messages" }).toBeLessThan(before);
    }
    expect(await withheld(page), "the newer messages never all paged in").toBe(0);
    await readToEnd();
    await expect(page.locator("[data-item-key]").last()).toContainText("newest after the burst");
    await expectSettledAtBottom(page, "the end of the paged range is not the newest message");
    await expect(page.getByTestId("scroll-to-bottom"), "the catch-up button is still shown at the newest message").toBeHidden();

    // Smoke check only: the scroll above re-measured the pin. The page-in
    // without a later settle is covered by the final-newer-page tests below.
    await deliver(page, "arrival after paging to the newest");
    await expectSettledAtBottom(page, "an arrival after paging to the newest message was not followed");
  });

  // A newer page slides the range's start past the ceiling. The reader's row
  // stays put through that page and the next arrival.
  test("a newer page keeps the reader's row", async ({ page }) => {
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1&deep-history-retention=1");
    await readerScrollsWithoutGesture(page, 1_000);
    await callRiverTest(page, "appendMessages", 450);
    await expect
      .poll(() => withheld(page), { message: "premise: the burst should hold more than one page" })
      .toBeGreaterThan(INITIAL_RENDERED_ITEMS);
    const before = await withheld(page);

    // One task: move into the trigger's reach and note the row in view,
    // before the page lands.
    const row = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      c.scrollTop = c.scrollHeight - c.clientHeight - 400;
      const cTop = c.getBoundingClientRect().top;
      for (const r of Array.from(c.querySelectorAll<HTMLElement>("[data-item-key]"))) {
        const rect = r.getBoundingClientRect();
        if (rect.height > 0 && rect.top >= cTop && rect.bottom <= cTop + c.clientHeight) {
          return { key: r.getAttribute("data-item-key")!, top: rect.top - cTop };
        }
      }
      return null;
    });
    expect(row, "premise: a row is fully in view").not.toBeNull();
    await expect.poll(() => withheld(page), { message: "premise: a newer page should land" }).toBeLessThan(before);
    await expectRowHeld(page, row!.key, row!.top, "a newer page moved the reader's row");

    const paged = await withheld(page);
    await callRiverTest(page, "appendMessage", "arrival after a newer page");
    await expect.poll(() => withheld(page), { message: "premise: the arrival should land" }).toBe(paged + 1);
    await expectRowHeld(page, row!.key, row!.top, "the arrival after a newer page moved the reader's row");
  });

  // While the scroll-to-latest button's smooth scroll is in flight the reader
  // reads as parked, so a burst past the ceiling holds the range's end. The
  // scroll then lands where the app recorded it, so the pin stays armed and
  // the reader is following. A following reader's range must reach the newest
  // message again; it used to keep the held end, and later arrivals never
  // rendered until the button was tapped again (#747 review, item 2).
  test("a later arrival renders and is followed after a burst during a smooth return", async ({ page }) => {
    // Past the ceiling from the opening window, leaving several pages withheld;
    // the retention variant keeps the room below its cap.
    const BURST = 450;
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1&deep-history-retention=1");
    expect(await withheld(page), "premise: the room opens on the latest range").toBe(0);
    // Below the backfill strip (top 800px), as in `parkPastTheCeiling`.
    await readerScrollsWithoutGesture(page, 1_000);
    const chevron = page.getByTestId("scroll-to-bottom");
    await expect(chevron, "premise: the reader is far enough up to be offered the button").toBeVisible();

    // Hold the button's smooth scroll. The app has already recorded its
    // destination and armed the pin when it calls `scrollTo`; keep that
    // destination (the bottom at call time) so the replay lands where the app
    // expects. Browser movement only: no application state is read or set.
    await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      const gate: { destination: number | null } = { destination: null };
      c.scrollTo = function (this: HTMLElement, ...args: unknown[]) {
        const opts = args[0] as ScrollToOptions | undefined;
        if (args.length === 1 && opts?.behavior === "smooth") {
          gate.destination = this.scrollHeight - this.clientHeight;
          return;
        }
        return (Element.prototype.scrollTo as (...a: unknown[]) => void).apply(this, args);
      } as typeof c.scrollTo;
      (window as any).__riverSmoothGate = gate;
    });
    try {
      await chevron.click();
      await expect
        .poll(() => page.evaluate(() => (window as any).__riverSmoothGate.destination), {
          message: "premise: the button should start a smooth scroll",
        })
        .not.toBeNull();
      const destination: number = await page.evaluate(() => (window as any).__riverSmoothGate.destination);
      const heldTop = await scrollTop(page);

      await callRiverTest(page, "appendMessages", BURST);
      // The newest arrivals are withheld, so check the oldest one instead.
      await expect(page.getByText("batched arrival 00", { exact: false })).toHaveCount(1, { timeout: 10_000 });
      const held = await withheld(page);
      expect(held, "premise: the burst should hold the end with more than one newer page withheld").toBeGreaterThan(
        INITIAL_RENDERED_ITEMS,
      );
      expect(Math.abs((await scrollTop(page)) - heldTop), "premise: nothing moved the view mid-return").toBeLessThanOrEqual(
        AT_BOTTOM_EPSILON_PX,
      );

      // Let the return finish, instantly, at the recorded destination.
      // Assigning `scrollTop` bypasses the held `scrollTo`.
      await readerScrollsWithoutGesture(page, destination);
      await nextFrames(page);
      expect(
        Math.abs((await scrollTop(page)) - destination),
        "premise: the return should land where the app recorded it",
      ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
      expect(await withheld(page), "premise: the return should page nothing in").toBe(held);
      expect(
        await distanceFromBottom(page),
        "premise: the burst grew the range below the return, out of the newer-history trigger's reach",
      ).toBeGreaterThan(NEWER_TRIGGER_REACH_PX);

      await callRiverTest(page, "appendMessage", "arrival after smooth-return burst");
      await expect(
        page.locator("[data-item-key]").last(),
        "a following reader's range kept its held end, so the later arrival never rendered",
      ).toContainText("arrival after smooth-return burst", { timeout: 5_000 });
      await expect
        .poll(() => withheld(page), { message: "newer messages are still withheld from a following reader" })
        .toBe(0);
      await expectSettledAtBottom(page, "the later arrival was not followed");
    } finally {
      await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container");
        if (c) delete (c as any).scrollTo;
        delete (window as any).__riverSmoothGate;
      });
    }
  });

  /// Fill the opening range to the render ceiling below a parked reader, then
  /// deliver `text`. Past the ceiling a parked reader's range holds its end
  /// (`HistoryWindow::resolve_held`), so exactly that message is withheld.
  async function holdOneNewerItem(page: Page, text: string) {
    // The retention variant keeps the burst from pruning under the range.
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1&deep-history-retention=1");
    // Below the backfill strip (top 800px), as in `parkPastTheCeiling`.
    await readerScrollsWithoutGesture(page, 1_000);
    const rendered = await page.locator("[data-item-key]").count();
    await callRiverTest(page, "appendMessages", RENDER_CEILING_ITEMS - rendered);
    await expect
      .poll(() => page.locator("[data-item-key]").count(), { message: "premise: the burst should fill the range to the ceiling" })
      .toBe(RENDER_CEILING_ITEMS);
    expect(await withheld(page), "premise: nothing is held at the ceiling").toBe(0);
    await callRiverTest(page, "appendMessage", text);
    await expect.poll(() => withheld(page), { message: "premise: one more arrival should hold the end" }).toBe(1);
    expect(
      await distanceFromBottom(page),
      "premise: the reader is out of the newer-history trigger's reach until they jump",
    ).toBeGreaterThan(NEWER_TRIGGER_REACH_PX);
  }

  /// Jump to the held range's end and settle there in ONE task, so the settle
  /// runs while `has_newer` is still true, before the newer-history trigger
  /// pages the last item in. The natural `scrollend` from the same move lands
  /// where this settle recorded, reads as the app's own, and leaves the pin.
  async function settleAtHeldEnd(page: Page) {
    await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      c.scrollTop = c.scrollHeight;
      c.dispatchEvent(new Event("scrollend"));
    });
  }

  // The reader settles at the end of a held range, which is not the newest
  // message, so the pin clears. The final newer page then lands below them
  // and nothing settles again. Inside the band they are back at the newest
  // message and must be following again (#747 review, item 1).
  test("following resumes after the final newer page without another settle", async ({ page }) => {
    await holdOneNewerItem(page, "short newest");
    await settleAtHeldEnd(page);
    await expect.poll(() => withheld(page), { message: "premise: the final newer page should land" }).toBe(0);
    await expect(
      page.locator("[data-item-key]").last(),
      "premise: the last item paged in rather than jumping to latest",
    ).toContainText("short newest");
    expect(await distanceFromBottom(page), "premise: the page-in left the reader inside the band").toBeLessThanOrEqual(
      BOTTOM_THRESHOLD_PX,
    );

    // Tall: a parked reader's arrival past the ceiling is held, then paged in
    // under a start slide, and the row that slide removes can leave a short
    // arrival at the bottom with no following at all.
    await callRiverTest(page, "appendMessage", `arrival after the final page ${"y".repeat(1_000)}`);
    await expect(
      page.locator("[data-item-key]").last(),
      "the arrival after the final newer page was not rendered",
    ).toContainText("arrival after the final page", { timeout: 5_000 });
    await expectSettledAtBottom(page, "an arrival after the final newer page was not followed");
  });

  // The same page-in, but tall enough to leave the reader outside the band:
  // they are reading, not at the newest message, so nothing follows.
  test("a final newer page that lands outside the band does not resume following", async ({ page }) => {
    await holdOneNewerItem(page, `tall newest ${"y".repeat(1_500)}`);
    await settleAtHeldEnd(page);
    await expect.poll(() => withheld(page), { message: "premise: the final newer page should land" }).toBe(0);
    await nextFrames(page);
    expect(
      await distanceFromBottom(page),
      "a page-in from outside the band took the reader to the bottom",
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);

    await deliverOffscreen(page, "arrival below the band");
    await nextFrames(page);
    expect(
      await distanceFromBottom(page),
      "an arrival after a page-in from outside the band was followed",
    ).toBeGreaterThan(BOTTOM_THRESHOLD_PX);
  });

  // A settle at the bottom schedules the trim for the next task. A reader who
  // moves in between must keep their rows.
  test("a trim scheduled at the bottom stands down when the reader moves before it runs", async ({ page }) => {
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1");
    // Backfill grows the window and leaves the reader near the top.
    await backfillOnce(page, "premise: backfill should grow the window past its initial size");
    const rows = await page.locator("[data-item-key]").count();
    expect(rows, "premise: the window is grown").toBeGreaterThan(INITIAL_RENDERED_ITEMS);

    // One task: land at the end, settle (schedules the trim), move up.
    const row = await page.evaluate(() => {
      const c = document.getElementById("chat-scroll-container")!;
      c.scrollTop = c.scrollHeight;
      c.dispatchEvent(new Event("scrollend"));
      c.scrollTop = c.scrollHeight - c.clientHeight - 1_500;
      const cTop = c.getBoundingClientRect().top;
      for (const r of Array.from(c.querySelectorAll<HTMLElement>("[data-item-key]"))) {
        const rect = r.getBoundingClientRect();
        if (rect.height > 0 && rect.top >= cTop && rect.bottom <= cTop + c.clientHeight) {
          return { key: r.getAttribute("data-item-key")!, top: rect.top - cTop };
        }
      }
      return null;
    });
    expect(row, "premise: a row is fully in view after moving up").not.toBeNull();

    await expectRowHeld(page, row!.key, row!.top, "a trim scheduled before the reader moved shifted their row");
    expect(await page.locator("[data-item-key]").count(), "the stale trim removed rows").toBe(rows);
  });

  // Tall older rows and a short retained tail. At an ordinary height the
  // measured tail clears the backfill strip, so the window trims once and stays
  // trimmed, and paging back still works.
  test("returning to the end after paging into tall rows trims once and stays bounded", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Uneven Tail Room", "/?uneven-history=1");
    const counts = await backfillThenReturn(page);
    expect(counts, "the window should trim back to its initial size exactly once").toEqual([INITIAL_RENDERED_ITEMS]);
    await expectSettledAtBottom(page, "the trim left the view off the end");

    await backfillOnce(page, "paging back after the trim revealed nothing");
  });
});

test.describe("Trimming on a very tall viewport (A05)", () => {
  // A zoomed-out window or tall portrait monitor: a trimmed window sits inside
  // the backfill strip's reach, so the measured tail skips the trim. The old
  // average-height estimate, inflated by the tall rows, trimmed anyway, and
  // trim and backfill looped on room open.
  test.use({ viewport: { width: 1280, height: 5_800 } });

  test("an uneven-height room settles instead of trimming and refilling", async ({ page }) => {
    await openRoomAtBottom(page, "Uneven Tail Room", "/?uneven-history=1");
    // Measured, not estimated: the height of the newest INITIAL_RENDERED_ITEMS rows.
    const tail = await page.evaluate((n) => {
      const c = document.getElementById("chat-scroll-container")!;
      const rows = c.querySelectorAll<HTMLElement>("[data-item-key]");
      return c.scrollHeight - rows[rows.length - n].offsetTop;
    }, INITIAL_RENDERED_ITEMS);
    expect(
      tail,
      "premise: a trimmed (initial-size, short) window must sit within the backfill strip's reach of the viewport",
    ).toBeLessThan((await viewportHeight(page)) + 800);

    const counts = await observeRowCounts(page);
    expect(
      counts.length,
      `the window kept trimming and refilling with no reader input (row counts: ${counts.slice(0, 8).join(" → ")}…)`,
    ).toBeLessThanOrEqual(1);
  });
});

test.describe("Own send while reading history (A06)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // CURRENT POLICY: your own message snaps the view to the latest message
  // (`force_scroll`), wherever you were reading. Preserving the reading position
  // instead would change the snap; the send and the draft clearing stay.
  test("sending while scrolled up submits, clears the draft and snaps to the latest message", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page, "premise: reading older messages");

    const input = page.getByTestId("message-input");
    await input.fill("own send while reading history");
    await input.press("Enter");

    await expect(input, "the draft was not cleared").toHaveValue("");
    await expect(
      page.locator("[data-item-key]").last(),
      "the sent message is not the newest row",
    ).toContainText("own send while reading history");
    await expectSettledAtBottom(page, "own send did not snap to the latest message (current policy)");
  });
});
