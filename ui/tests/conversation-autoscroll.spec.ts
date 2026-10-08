import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { roomUnreadBadge, selectListedRoom } from "./example-room";
import {
  AT_BOTTOM_EPSILON_PX,
  HISTORY_ROWS,
  READING_ROW_BUDGET_PX,
  WELL_AWAY_FROM_END_PX,
  deliver,
  deliverOffscreen,
  distanceFromBottom,
  expectMessageInView,
  expectParkedAwayFromEnd,
  expectReadingRow,
  expectHeldFromViewBottom,
  expectRowHeld,
  expectSettleWithheld,
  expectSettledAtBottom,
  expectStaysPut,
  fillHistory,
  historyHeight,
  holdSettleEvents,
  holdTestImage,
  newestRowFromViewBottom,
  nextFrames,
  openRoomAtBottom,
  readerLeavesAndReturnsToEnd,
  readerScrollsTo,
  readerScrollsWithoutGesture,
  readingRow,
  releaseSettleEvents,
  rowTop,
  scrollTop,
  settle,
  viewportHeight,
  withheld,
} from "./history-geometry";

/// A draft long enough to take more than WELL_AWAY_FROM_END_PX off the history.
const LONG_DRAFT = Array.from({ length: 12 }, (_, i) => `draft line ${i}`).join("\n");

// Arrivals never move the view, wherever the reader is, the very end of the
// history included. The Latest button is how a reader reaches
// what arrived below them.
// Rationale: .claude/rules/history-scrolling.md.
//
// These began as the freenet/river#486 tests. #486 was the view NOT following
// new messages: the follow was gated on a 1px sentinel with a 100px
// `rootMargin`, and once the gap passed 100px nothing re-armed it. On the live
// Freenet room that meant 54 arrivals with the view frozen while the gap
// ratcheted from 147px to 2725px. Following has since been dropped on purpose,
// so each test keeps #486's events (the composer taking the bottom off screen, a
// mid-list insert, the reader leaving and coming back, layout-only growth) and
// asserts the opposite outcome.
//
// "The view did not move" is measured on the row the reader is looking at
// (`expectRowHeld`, within READING_ROW_BUDGET_PX), never on the distance from
// the end, which grows with every arrival whether or not the view moved. When
// the chat area itself changes height (the composer), the view keeps its
// BOTTOM edge instead, so those steps measure the newest
// message against the bottom of the view (`expectHeldFromViewBottom`).
//
// Assumes the example-data build, which exposes `window.__riverTest` for
// delivering INBOUND messages. Sending through the composer is an explicit
// request to go to the newest message, so it proves nothing about arrivals; A06
// below covers it.

test.describe("Arrivals never move the view (#486)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a burst of arrivals does not move a reader at the end while the composer takes the bottom off screen", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const roomyViewport = await viewportHeight(page);
    const newest = await newestRowFromViewBottom(page);

    // Typing a long message grows the composer, which takes more than 100px off
    // the history in one step. That latched #486 with no network activity at
    // all, which is why #468 (the composer auto-resize) was one of its causes.
    await page.getByTestId("message-input").fill(LONG_DRAFT);

    // The premise, asserted rather than assumed: the window over the history
    // has to shrink by more than the old margin.
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message:
          "premise: the composer did not grow by more than the old 100px " +
          "margin, so this test is not exercising #486's latch",
      })
      .toBeLessThan(roomyViewport - WELL_AWAY_FROM_END_PX);

    // The window shrinks from the bottom and the view keeps its bottom edge,
    // so the newest message stays where it was above the composer.
    await expectHeldFromViewBottom(page, newest.key, newest.gap, "the composer grew and the newest message moved");

    // #486's recorded failure was a run of arrivals; a loop catches a view that
    // stays put for one arrival and moves on a later one.
    for (let i = 1; i <= 2; i++) {
      await deliverOffscreen(page, `draft-open arrival ${i}`);
      await expectHeldFromViewBottom(
        page,
        newest.key,
        newest.gap,
        `arrival ${i} landed while a draft was open and moved the view`,
      );
    }

    // Clearing the draft gives the height back; the bottom edge still holds,
    // now with the arrivals below it.
    await page.getByTestId("message-input").fill("");
    await expect
      .poll(() => viewportHeight(page), { timeout: 5_000 })
      .toBeGreaterThan(roomyViewport - WELL_AWAY_FROM_END_PX);
    await expectHeldFromViewBottom(page, newest.key, newest.gap, "the composer collapsed and the view lost its bottom edge");
    await deliverOffscreen(page, "arrived after the draft was cleared");
    await expectHeldFromViewBottom(
      page,
      newest.key,
      newest.gap,
      "the draft was cleared, a message arrived, and the view moved",
    );
    await expect(
      page.getByTestId("scroll-to-bottom"),
      "the arrivals below the view must stay reachable",
    ).toBeVisible();
  });

  // Each correction is measured from the reading position captured at the last
  // settle, so several resizes before the next settle must not compound.
  test("a draft typed line by line before any settle keeps the newest message where it was", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const roomyViewport = await viewportHeight(page);
    const newest = await newestRowFromViewBottom(page);
    const input = page.getByTestId("message-input");

    await holdSettleEvents(page);
    const lines: string[] = [];
    for (let i = 0; i < 12; i++) {
      lines.push(`draft line ${i}`);
      await input.fill(lines.join("\n"));
      await nextFrames(page);
    }
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message: "premise: the draft should take more than the old margin off the history",
      })
      .toBeLessThan(roomyViewport - WELL_AWAY_FROM_END_PX);
    await expectSettleWithheld(page);
    await expectHeldFromViewBottom(page, newest.key, newest.gap, "a draft grown one line at a time moved the newest message");
    await releaseSettleEvents(page);
    await expectHeldFromViewBottom(page, newest.key, newest.gap, "the late settle moved the newest message");
  });

  // A settle that lands in the same frame as the composer growing, before the
  // ResizeObserver has seen the new height, must not capture the reading
  // position over the view the observer is about to correct.
  test("a settle in the same frame the composer grows keeps the newest message where it was", async ({ page }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const hasScrollend = await page.evaluate(() => "onscrollend" in document.getElementById("chat-scroll-container")!);
    // Without `scrollend` the settle is a 120ms debounce, never in the frame.
    test.skip(!hasScrollend, "no scrollend: the settle cannot land in the same frame");
    await readerLeavesAndReturnsToEnd(page);
    const roomyViewport = await viewportHeight(page);
    const newest = await newestRowFromViewBottom(page);

    // On `document`, so it runs after the app's own input handler has grown
    // the composer. Reading clientHeight lays the growth out now, and the
    // settle runs in the same task, ahead of any ResizeObserver callback.
    await page.evaluate((shrunkBelow) => {
      const c = document.getElementById("chat-scroll-container")!;
      document.addEventListener(
        "input",
        () => {
          const shrunk = c.clientHeight < shrunkBelow;
          c.dispatchEvent(new Event("scrollend"));
          (window as any).__riverSameFrameSettle = { shrunk };
        },
        { once: true },
      );
    }, roomyViewport - WELL_AWAY_FROM_END_PX);
    await page.getByTestId("message-input").fill(LONG_DRAFT);

    const premise = await page.evaluate(() => (window as any).__riverSameFrameSettle);
    expect(premise?.shrunk, "premise: the composer should grow inside the input handler").toBe(true);
    await expectHeldFromViewBottom(
      page,
      newest.key,
      newest.gap,
      "a settle in the same frame the composer grew lost the bottom edge",
    );
  });

  test("a mid-list insert that does not remount the last row does not move a reader at the end", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    const row = await expectReadingRow(page);
    const heightBefore = await historyHeight(page);

    // Tag the last row so we can prove afterwards that it was diffed, not
    // remounted. A remount is the old `onmounted` path; a diff is the content
    // change this test is about.
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
    await expect(
      page.getByText("inserted above the last row", { exact: false }),
      "premise: the insert should land",
    ).toHaveCount(1, { timeout: 5_000 });
    expect(
      await historyHeight(page),
      "premise: the insert should make the history taller",
    ).toBeGreaterThan(heightBefore);

    await expectRowHeld(page, row.key, row.top, "content grew above the last row and the view moved");

    const lastRowSurvived = await page
      .locator(HISTORY_ROWS)
      .last()
      .evaluate((row) => (row as any).__riverProbe === "last-row");
    expect(
      lastRowSurvived,
      "the last row remounted, so this exercised a remount rather than the " +
        "in-place content change it is meant to pin"
    ).toBe(true);
  });

  test("an arrival does not move the view whether the reader is parked, back at the end, or brought there by Latest", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);

    await readerScrollsTo(page, 0);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(WELL_AWAY_FROM_END_PX);

    await deliver(page, "arrived while reading history");
    await expectStaysPut(
      page,
      "a message arrived while the reader was scrolled up and yanked the view"
    );

    // The reader scrolls back to the end THEMSELVES. Under following, this
    // settle re-armed the pin; now it must change nothing about what the next
    // arrival does.
    await readerScrollsWithoutGesture(page, await historyHeight(page));
    await expectSettledAtBottom(page, "the reader's own scroll should reach the bottom");
    const back = await expectReadingRow(page);
    await deliverOffscreen(page, "arrived after the reader scrolled back down");
    await expectRowHeld(
      page,
      back.key,
      back.top,
      "the reader returned to the end and an arrival moved the view",
    );

    // Latest is a second, independent way back, and it does not start
    // following either.
    await readerScrollsTo(page, 0);
    await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({
      timeout: 5_000,
    });
    await page.getByTestId("scroll-to-bottom").click();
    await expectSettledAtBottom(page, "the scroll-to-latest button should reach the bottom");
    const latest = await expectReadingRow(page);
    await deliverOffscreen(page, "arrived after the button was used");
    await expectRowHeld(
      page,
      latest.key,
      latest.top,
      "an arrival after Latest moved the view: Latest started following",
    );
  });
});

test.describe("Layout-only growth does not move the view (#486)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("a resize that reflows the history does not take a reader at the end to the newest message", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    // A reader back at the end by themselves: right after the opening, the
    // end is held through height changes (covered below).
    await readerLeavesAndReturnsToEnd(page);

    const before = await historyHeight(page);
    const topBefore = await scrollTop(page);
    const viewBefore = await viewportHeight(page);

    // Narrowing the window makes every message wrap onto more lines, so the
    // history gets taller and its end drops below the fold. No state changed,
    // so the grouped-message memo does not re-run; only a ResizeObserver sees
    // this. It is the same class as a late-loading image or a font swapping in.
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
      .toBeGreaterThan(before + WELL_AWAY_FROM_END_PX);

    // A reflow rewraps every row, so no row keeps its offset. What must hold
    // is the scroll position (the view stays anchored at its top), rather than
    // a jump to the newest message. Width alone gets no correction; where the
    // narrower layout also changes the chat area's height (mobile-safari:
    // 762px -> 754px), the bottom edge holds through that change, moving the
    // scroll position by exactly the difference.
    await nextFrames(page);
    await page.waitForTimeout(600);
    const heightChange = viewBefore - (await viewportHeight(page));
    expect(
      Math.abs((await scrollTop(page)) - topBefore - heightChange),
      `the history reflowed taller and the view was moved (chat area height change: ${heightChange}px)`,
    ).toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
  });

  test("history that grows in the same frame the composer collapses does not move a reader at the end", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    // Not held at the end by the opening, which would take
    // the view to the grown row's end and hide what this test measures.
    await readerLeavesAndReturnsToEnd(page);
    const roomyViewport = await viewportHeight(page);
    // Its TOP: the row grows below the bottom edge, and content growth is not
    // followed.
    const newest = await newestRowFromViewBottom(page, "top");
    await page.getByTestId("message-input").fill(LONG_DRAFT);
    await expect
      .poll(() => viewportHeight(page), {
        timeout: 5_000,
        message:
          "premise: the composer did not grow enough that clearing it would " +
          "clamp the view, so this test is not exercising the same-frame race",
      })
      .toBeLessThan(roomyViewport - WELL_AWAY_FROM_END_PX);
    await expectHeldFromViewBottom(page, newest.key, newest.gap, "the composer grew and the newest message moved", "top");

    // On `document`, so it runs after the app's own input handler has
    // collapsed the composer, before any frame. Reading clientHeight there
    // makes the browser clamp the view before the row grows, so a correction
    // computed from `scrollTop` would count the collapse twice.
    const GROWTH_PX = 80;
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
      { grow: GROWTH_PX, collapsedAbove: roomyViewport - WELL_AWAY_FROM_END_PX },
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
    await expectHeldFromViewBottom(
      page,
      newest.key,
      newest.gap,
      "the history grew in the same frame the composer collapsed and the newest message moved",
      "top",
    );
  });
});

// Regression tests for freenet/river#501: the #498 windowed tail slid its
// start index forward on every arrival, removing the oldest rendered rows in
// the same patch that appended the new message. Browser scroll anchoring
// rewrote scrollTop to hold the visible content still, `reader_moved_up_since`
// attributed the browser's adjustment to the reader, and both follow paths
// stood down — so a room deeper than the render window stopped following
// arrivals entirely, while every room the old suite seeded (~15-20 items vs a
// 60-item window) kept passing on the pre-window code path. With following
// gone, the window must still grow rather than slide, so the reader's rows
// stay where they are.
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
test.describe("Windowed history keeps the reader's place through arrivals (#501)", () => {
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

  test("a burst of arrivals in a windowed room does not move a reader at the end", async ({
    page,
  }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    await expectWindowedRenderActive(page);
    const row = await expectReadingRow(page);
    const items = page.locator("[data-item-key]");
    const headKey = await items.first().getAttribute("data-item-key");
    const itemsBefore = await items.count();

    // The #501 failure mode with the reader at the end: each arrival slid the
    // window, removing rows above the view in the same patch. The loop catches
    // a view that holds for one arrival and moves on a later one. Delivered
    // arrivals alternate authors (see `test_author` in test_hooks.rs), so each
    // one is its own display item; six arrivals folding into one group would
    // exercise the windowing arithmetic zero times.
    for (let i = 1; i <= 6; i++) {
      await deliverOffscreen(page, `windowed arrival ${i}`);
      await expectRowHeld(
        page,
        row.key,
        row.top,
        `arrival ${i} in a windowed room moved a reader at the end`,
      );
    }

    // The window grew to hold the arrivals instead of sliding its head. A
    // reader who never scrolls produces no settle, so nothing trims it either;
    // the render ceiling bounds it (the A04 idle-reader case below).
    await expect(items.first(), "the window's head slid when arrivals landed").toHaveAttribute(
      "data-item-key",
      headKey!,
    );
    expect(await items.count(), "the window should grow by exactly the arrivals").toBe(
      itemsBefore + 6,
    );
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
      .toBeGreaterThan(WELL_AWAY_FROM_END_PX);

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

    // Park just far enough up to leave the end, NOT mid-history: the 61-message
    // batch drains ~30 display items off the FRONT of a 74-item room, so a
    // row tagged mid-history is inside the pruned range and legitimately
    // leaves the DOM — which row exactly depends on per-engine row heights,
    // so tagging there is flaky by construction rather than by timing. The
    // rows just above the fold are the newest ones; they survive the drain,
    // and holding THEM still is the property under test.
    //
    // Deliberately NO settle wait before delivering: the batch lands while
    // the reader's `scrollend` is still in flight, so the reposition has to
    // hold the rows without a settle having recorded where the reader is.
    const parkedAt = Math.max(
      0,
      (await historyHeight(page)) - (await viewportHeight(page)) - 400
    );
    await readerScrollsTo(page, parkedAt);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(WELL_AWAY_FROM_END_PX);

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

    // Park mid-history so the batch below grows the window (a settle at the
    // end would trim the divergence away before paging).
    const mid = Math.floor((await historyHeight(page)) / 2);
    await readerScrollsTo(page, mid);
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 5_000 })
      .toBeGreaterThan(WELL_AWAY_FROM_END_PX);

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
// (A01), the render ceiling and uneven-row trim (A04, A05), and own send (A06).
// The others are in other specifications: content above a parked reader and a
// deleted reading row (A02) and a hidden panel (A03) in
// conversation-history-position.spec.ts, unseen arrivals, Latest and read state
// (A07) in room-unread-badge.spec.ts, and edit-form reachability (A08) in
// message-layout.spec.ts. They encode explicit navigation: the view moves
// only when the reader asks (opening a room, sending, Latest).

// Fixture assumptions from ui/src/example_data.rs, named where they are consumed.
/// Rows the history renders when a room opens, and trims back to.
const INITIAL_RENDERED_ITEMS = 60;
/// How far arrivals alone may grow the rendered range from an opening window
/// (WINDOW_ITEMS_CEILING in conversation.rs).
const ARRIVAL_CEILING_ITEMS = INITIAL_RENDERED_ITEMS * 4;
/// Tall rows at the head of "Tall Head Room" (at its message cap).
const TALL_HEAD_TALL_ROWS = 8;
/// Tall rows at the head of "Uneven Tail Room", numbered 00 up.
const UNEVEN_TAIL_TALL_ROWS = 40;

test.describe("Arrival before the reader's settle (A01)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // freenet/river#723 asked for an arrival patched between "back at the end"
  // and the reader's settle to be followed. Arrivals no longer move the view,
  // so the same move → patch → settle order now pins that the late settle does
  // not turn into a navigation either.
  test("an arrival patched before the reader's return-to-bottom settles does not move the view", async ({
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
    const row = await expectReadingRow(page);
    await deliverOffscreen(page, "arrived before the reader's settle");
    await nextFrames(page);
    await releaseSettleEvents(page);

    await expectRowHeld(
      page,
      row.key,
      row.top,
      "the reader returned to the end, a message landed before their settle, and the view moved",
    );
  });

  // freenet/river#508. The reader scrolls up and, before their settle, a burst
  // drains TALL rows from the head of an at-cap room, so the history shrinks
  // under them and the browser clamps their offset. The follow pin, still
  // stale-true, used to read that clamp as "not moved" and take the view to
  // the end. Invariant: the row they scrolled to stays where it is.
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

/// The Deep History Room fixture: 188 fillers + 13 standard messages, capped
/// at DEEP_ROOM_MAX_RECENT_MESSAGES.
const DEEP_ROOM_SEEDED = 201;
const DEEP_ROOM_CAP = 300;
/// A burst that passes the render ceiling from the room's opening window: its
/// prune (201 + 200 - 300 = 101 messages) stays above that window's start.
const CEILING_BURST = 200;

/// Park in the Deep History Room, then send a 200-message burst that passes
/// the ceiling and prunes the oldest messages, but not the reader's.
async function parkPastTheCeiling(page: Page) {
  await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1");
  // Below the backfill strip (top 800px), near the head of the window.
  await readerScrollsWithoutGesture(page, 1_000);
  const row = await readingRow(page, "history filler");
  expect(row, "premise: a filler row is fully in view").not.toBeNull();
  const fillerIndex = Number(/history filler (\d+)/.exec(row!.text)![1]);

  const pruned = DEEP_ROOM_SEEDED + CEILING_BURST - DEEP_ROOM_CAP;
  expect(
    fillerIndex,
    "premise: the reader's message survives the prune, so it is still in the room",
  ).toBeGreaterThanOrEqual(pruned);
  await callRiverTest(page, "appendMessages", CEILING_BURST);
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
  await expectRowHeld(page, row!.key, row!.top, "a burst past the render ceiling took the reader off a message the room still holds");
  return row!;
}

test.describe("Render ceiling and trimming (A04, A05)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

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

  // With no following, a reader idle at the exact end never scrolls, so
  // nothing settles and nothing trims. The ceiling is then what bounds the DOM:
  // the range's end holds and newer items are withheld, below the reader.
  test("bursts past the ceiling leave an idle reader at the exact end in place with a bounded DOM", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1");
    expect(await distanceFromBottom(page), "premise: the reader is at the exact end").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    const row = await expectReadingRow(page);

    await callRiverTest(page, "appendMessages", CEILING_BURST);
    await expect(page.getByText("batched arrival 00", { exact: false }), "premise: the burst should land").toHaveCount(1, {
      timeout: 10_000,
    });
    await expectRowHeld(page, row.key, row.top, "a burst moved a reader idle at the end");
    await expect
      .poll(() => withheld(page), { message: "premise: the burst should take the range past the ceiling" })
      .toBeGreaterThan(0);
    const rows = await page.locator("[data-item-key]").count();
    expect(rows, "arrivals grew the DOM past the render ceiling").toBeLessThanOrEqual(ARRIVAL_CEILING_ITEMS);

    // Small enough that its prune stays above the rendered range.
    const before = await withheld(page);
    await callRiverTest(page, "appendMessages", 20);
    await expect.poll(() => withheld(page), { message: "premise: the second burst should land" }).toBeGreaterThan(before);
    await callRiverTest(page, "appendMessage", "newest after the idle bursts");
    await expect.poll(() => withheld(page), { message: "premise: the last arrival should land" }).toBeGreaterThan(before + 20);
    await expectRowHeld(page, row.key, row.top, "a burst below a held range moved an idle reader");
    expect(await page.locator("[data-item-key]").count(), "arrivals below a held range grew the DOM").toBe(rows);

    // The withheld messages stay reachable.
    await expect(page.getByTestId("scroll-to-bottom"), "the withheld messages must be reachable").toBeVisible();
    await page.getByTestId("scroll-to-bottom").click();
    await expect(page.locator("[data-item-key]").last(), "jump to latest did not reach the newest message").toContainText(
      "newest after the idle bursts",
    );
    await expectSettledAtBottom(page, "jump to latest did not land at the newest message");
    expect(await withheld(page), "the latest range still withholds newer items").toBe(0);
  });

  // A settle at the bottom schedules the trim for the next task. A reader who
  // moves in between must keep their rows. The 5px case is just outside the
  // 2px rounding slack and rejects a widened near-bottom trim allowance.
  for (const distance of [5, 1_500]) {
    test(`a trim scheduled at the bottom stands down when the reader moves ${distance}px before it runs`, async ({ page }) => {
      await openRoomAtBottom(page, "Deep History Room", "/?deep-history-room=1");
      // Backfill grows the window and leaves the reader near the top.
      await backfillOnce(page, "premise: backfill should grow the window past its initial size");
      const rows = await page.locator("[data-item-key]").count();
      expect(rows, "premise: the window is grown").toBeGreaterThan(INITIAL_RENDERED_ITEMS);

      // One task: land at the end, settle (schedules the trim), move up.
      const row = await page.evaluate((distance) => {
        const c = document.getElementById("chat-scroll-container")!;
        c.scrollTop = c.scrollHeight;
        c.dispatchEvent(new Event("scrollend"));
        c.scrollTop = c.scrollHeight - c.clientHeight - distance;
        const cTop = c.getBoundingClientRect().top;
        for (const r of Array.from(c.querySelectorAll<HTMLElement>("[data-item-key]"))) {
          const rect = r.getBoundingClientRect();
          if (rect.height > 0 && rect.top >= cTop && rect.bottom <= cTop + c.clientHeight) {
            return {
              key: r.getAttribute("data-item-key")!,
              top: rect.top - cTop,
              distance: c.scrollHeight - c.scrollTop - c.clientHeight,
            };
          }
        }
        return null;
      }, distance);
      expect(row, "premise: a row is fully in view after moving up").not.toBeNull();
      expect(row!.distance, "premise: the reader moved the intended distance from the end").toBeCloseTo(distance, 0);

      await expectRowHeld(page, row!.key, row!.top, "a trim scheduled before the reader moved shifted their row");
      expect(await page.locator("[data-item-key]").count(), "the stale trim removed rows").toBe(rows);

      // Prove the grown window is eligible to trim once the reader returns.
      await readerScrollsWithoutGesture(page, await historyHeight(page));
      await expect.poll(() => page.locator("[data-item-key]").count(), {
        message: "returning to the exact bottom should trim the grown window",
      }).toBe(INITIAL_RENDERED_ITEMS);
      await expectSettledAtBottom(page, "the trim left the view off the end");
    });
  }

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

// Your own message takes the view to the end of the history once, from
// wherever you were reading, after the send applies locally.
// It does not start following, and a send that fails to apply goes nowhere.
test.describe("Own send goes to the newest message once (A06)", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  /// Send `text` with Enter, the way a reader does.
  async function sendWithEnter(page: Page, text: string) {
    const input = page.getByTestId("message-input");
    await input.fill(text);
    await input.press("Enter");
    await expect(input, "the draft was not cleared").toHaveValue("");
  }

  /// The view is at the end of the history with `text`'s message entirely
  /// above the composer.
  async function expectAtEndShowing(page: Page, text: string, why: string) {
    await expectSettledAtBottom(page, why);
    await expectMessageInView(page, text, why);
  }

  async function expectNewestRow(page: Page, text: string) {
    await expect(page.locator("[data-item-key]").last(), "the sent message is not the newest row").toContainText(text);
  }

  test("sending from the top of the history submits, clears the draft and goes to the latest message once", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page, "premise: reading older messages");

    await sendWithEnter(page, "own send while reading history");

    await expectNewestRow(page, "own send while reading history");
    await expectAtEndShowing(page, "own send while reading history", "a send from the top did not go to the latest message");

    const row = await expectReadingRow(page);
    await deliverOffscreen(page, "arrived after the send");
    await expectRowHeld(page, row.key, row.top, "an arrival after a send moved the view: the send started following");
  });

  test("sending from a held range selects the latest range and goes to the end", async ({ page }) => {
    await parkPastTheCeiling(page);
    await callRiverTest(page, "appendMessage", "newest before the send");
    await expect.poll(() => withheld(page), { message: "premise: newer messages are held back" }).toBeGreaterThan(0);
    await expect(
      page.getByText("newest before the send", { exact: false }),
      "premise: the newest message is outside the held range",
    ).toHaveCount(0);

    await sendWithEnter(page, "own send from a held range");

    await expectNewestRow(page, "own send from a held range");
    await expectAtEndShowing(page, "own send from a held range", "a send from a held range did not go to the end");
    expect(await withheld(page), "the range after the send still withholds newer items").toBe(0);
    await expect(
      page.getByText("newest before the send", { exact: false }),
      "the messages that were held back are not in the latest range",
    ).toHaveCount(1);
  });

  // A send's scroll belongs to the room it was sent in. A switch that lands
  // after the keypress but before the send applies leaves the new room to its
  // own opening, and a later arrival there must not finish the old room's send.
  test("a room switch between the send and its render cancels the send's scroll", async ({ page }) => {
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page, "other room");
    await selectListedRoom(page, "Team Chat Room");
    await expectSettledAtBottom(page, "premise: the room opens at its newest message");
    await page.getByTestId("message-input").fill("own send before a room switch");

    // One task: the keypress starts the send, and the switch is queued ahead
    // of the send's local apply, which runs two task hops later.
    await page.evaluate(() => {
      document
        .getElementById("message-input")!
        .dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
      (window as any).__riverTest.switchRoom("Your Private Room");
    });
    await expect(page.getByRole("heading", { name: "Your Private Room" })).toBeVisible();
    await expectSettledAtBottom(page, "the room switched to did not open at its newest message");

    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page, "premise: reading older messages in the room switched to");
    const row = await expectReadingRow(page);
    await deliverOffscreen(page, "arrival after the room switch");
    await expectRowHeld(page, row.key, row.top, "an arrival in the new room finished the old room's send");

    // Checked last, so it cannot disturb the scenario: the send did apply.
    await selectListedRoom(page, "Team Chat Room");
    await expect(
      page.getByText("own send before a room switch", { exact: false }),
      "premise: the send should have applied in the room it was sent from",
    ).toHaveCount(1);
  });

  test("a send that fails to apply does not navigate", async ({ page }) => {
    const applyFailures: string[] = [];
    page.on("console", (msg) => {
      if (msg.text().includes("Failed to apply message delta")) applyFailures.push(msg.text());
    });
    // Self owns this room, which the hook below needs.
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page, "premise: reading older messages");
    const row = await expectReadingRow(page);

    // A private room whose secret this device lacks: the send falls back to a
    // public body, which the local apply rejects after the draft is handed off.
    await callRiverTest(page, "makeRoomPrivateWithoutSecret");
    await expect(
      page.getByTestId("room-list").getByRole("button", { name: "Your Private Room" }).getByLabel("Private room"),
      "premise: the room should now be private",
    ).toBeVisible();
    await sendWithEnter(page, "own send that fails to apply");
    await expect
      .poll(() => applyFailures.length, {
        message: "premise: the send should reach the local apply and be rejected there",
      })
      .toBeGreaterThan(0);

    await expectRowHeld(page, row.key, row.top, "a send that failed to apply moved the view");
    await expect(
      page.getByText("own send that fails to apply", { exact: false }),
      "premise: the send should have failed to apply",
    ).toHaveCount(0);
    await expectParkedAwayFromEnd(page, "a send that failed to apply took the reader to the latest message");
  });

  // Another member's clock may run up to CLOCK_SKEW_TOLERANCE_SECS (60s) ahead
  // of ours, and their message then sorts BELOW one we send afterwards. The
  // send still goes to the true end, and the sent message stays in view
  // because the later message fits under it.
  test("a send goes to the true end when another member's message is stamped ahead of our clock", async ({
    page,
  }) => {
    await openRoomAtBottom(page, "Team Chat Room");
    await fillHistory(page);
    await callRiverTest(page, "appendMessageAhead", "stamped ahead of our clock", 50);
    await expect(
      page.getByText("stamped ahead of our clock", { exact: false }),
      "premise: the skewed message should land",
    ).toHaveCount(1, { timeout: 5_000 });
    await readerScrollsWithoutGesture(page, 0);
    await expectParkedAwayFromEnd(page, "premise: reading older messages");

    await sendWithEnter(page, "own send under clock skew");

    await expect(
      page.locator("[data-item-key]").last(),
      "premise: the message stamped ahead should sort below the one sent after it",
    ).toContainText("stamped ahead of our clock");
    await expectAtEndShowing(
      page,
      "own send under clock skew",
      "a send under clock skew did not go to the true end with the sent message in view",
    );
  });
});

// Once an explicit request (opening a room, an own send,
// Latest) has put the reader at the end, the end stays in view while rows
// change height (a private room's placeholders decrypting, late images, font
// swaps), until the reader first scrolls away. An arrival ends the hold
// instead of moving the view, as arrivals never do.
//
// The growing row is a Markdown image whose request is held until the test
// lets it load, two short messages above the end.
test.describe("The end holds after an explicit request", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  const imageRow = (page: Page) => page.locator("[data-item-key]", { hasText: "image fixture" });

  /// An image row, still loading, two short messages above the end of the
  /// open room. All three arrive below the reader, so the room is unread.
  async function addLoadingImageAboveTheEnd(page: Page, image: { requested: () => number }) {
    // The Markdown renders to text plus an <img>, so wait on the plain words.
    await callRiverTest(page, "appendMessage", "image fixture ![fixture](/test-image.svg) above the end");
    await expect(imageRow(page)).toHaveCount(1, { timeout: 5_000 });
    await deliverOffscreen(page, "below the image 1");
    await deliverOffscreen(page, "below the image 2");
    await expect.poll(image.requested, { message: "premise: the image is requested and held" }).toBeGreaterThan(0);
  }

  /// Let the image load, and wait for its row to grow.
  async function loadImage(page: Page, image: { release: () => Promise<void> }) {
    const height = () => imageRow(page).evaluate((r) => r.getBoundingClientRect().height);
    const before = await height();
    await image.release();
    await expect
      .poll(height, { message: "premise: the image row should grow when the image lays out" })
      .toBeGreaterThan(before + 50);
  }

  const teamChatBadge = (page: Page) => roomUnreadBadge(page, "Team Chat Room");

  /// Leave Team Chat and open it again: an opening request, with the image
  /// still loading in view above the end.
  async function reopenTeamChat(page: Page) {
    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "premise: the messages below the reader left Team Chat unread").toBeVisible();
    await selectListedRoom(page, "Team Chat Room");
    await page.mouse.move(0, 0);
    await expectSettledAtBottom(page, "premise: the room opens at its newest message");
    const imageTop = await rowTop(page, (await imageRow(page).getAttribute("data-item-key"))!);
    expect(imageTop, "premise: the image row is in view").toBeGreaterThan(0);
  }

  test("a row growing above the end after the room opens keeps the newest message in view, and the room read", async ({
    page,
  }) => {
    const image = await holdTestImage(page);
    await openRoomAtBottom(page, "Team Chat Room");
    await addLoadingImageAboveTheEnd(page, image);
    await reopenTeamChat(page);

    await loadImage(page, image);

    await expectSettledAtBottom(page, "a row grew above the end after the room opened and left the newest message off screen");
    await expect(page.getByTestId("scroll-to-bottom"), "Latest offered at the end").toHaveCount(0);
    await selectListedRoom(page, "Public Discussion Room");
    await expect(teamChatBadge(page), "the room opened at its newest message and still counts it unread").toHaveCount(0);
  });

  test("once the reader scrolls away, the same growth does not move them", async ({ page }) => {
    const image = await holdTestImage(page);
    await openRoomAtBottom(page, "Team Chat Room");
    await addLoadingImageAboveTheEnd(page, image);
    await reopenTeamChat(page);
    await readerScrollsWithoutGesture(page, (await scrollTop(page)) - 150);
    await expectParkedAwayFromEnd(page, "premise: the reader scrolled away from the end");
    const row = await expectReadingRow(page);
    expect(
      (await rowTop(page, (await imageRow(page).getAttribute("data-item-key"))!))!,
      "premise: the image row is below the reading row",
    ).toBeGreaterThan(row.top);

    await loadImage(page, image);

    await expectRowHeld(page, row.key, row.top, "a row grew after the reader scrolled away and the view moved");
  });

  test("an arrival after the room opens does not move the view, and ends the hold", async ({ page }) => {
    const image = await holdTestImage(page);
    await openRoomAtBottom(page, "Team Chat Room");
    await addLoadingImageAboveTheEnd(page, image);
    await reopenTeamChat(page);
    const row = await expectReadingRow(page);

    await deliverOffscreen(page, "arrived after the opening");
    await expectRowHeld(page, row.key, row.top, "an arrival after the opening moved the view");
    const before = await scrollTop(page);
    await loadImage(page, image);

    await settle(page);
    expect(await scrollTop(page), "a row grew after an arrival and the view went to the end").toBeCloseTo(before, 0);
    await expect(page.getByTestId("scroll-to-bottom"), "the arrival must stay reachable").toBeVisible();
  });

  test("Latest holds the end while a row above it grows", async ({ page }) => {
    const image = await holdTestImage(page);
    await openRoomAtBottom(page, "Team Chat Room");
    await addLoadingImageAboveTheEnd(page, image);
    await readerScrollsWithoutGesture(page, 0);
    await page.getByTestId("scroll-to-bottom").click();
    await expectSettledAtBottom(page, "premise: Latest reaches the end");

    await loadImage(page, image);

    await expectSettledAtBottom(page, "a row grew above the end after Latest and left the newest message off screen");
  });

  test("an own send holds the end while a row above it grows", async ({ page }) => {
    const image = await holdTestImage(page);
    await openRoomAtBottom(page, "Team Chat Room");
    await addLoadingImageAboveTheEnd(page, image);
    await readerScrollsWithoutGesture(page, 0);
    const input = page.getByTestId("message-input");
    await input.fill("own send before the image loads");
    await input.press("Enter");
    await expect(input, "the draft was not cleared").toHaveValue("");
    await expect(page.locator("[data-item-key]").last()).toContainText("own send before the image loads");
    await expectSettledAtBottom(page, "premise: the send goes to the end");

    await loadImage(page, image);

    await expectSettledAtBottom(page, "a row grew above the end after a send and left the newest message off screen");
  });
});
