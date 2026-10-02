import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { selectListedRoom, waitForApp } from "./example-room";
import {
  savedVisibleRowDrift,
  newestVisibleRow,
  registerHistoryGeometry,
  type RowPosition,
} from "./history-scroll-geometry";
import {
  AT_BOTTOM_EPSILON_PX,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  expectSettledAtBottom,
  fillHistory,
  historyHeight,
  openRoomAtBottom,
  readerScrollsWithoutGesture,
  viewportHeight,
} from "./history-scroll-helpers";

// Where a room's view is when the reader gets to it, and that nothing but the
// reader moves it afterwards (ui/src/components/conversation/history_scroll.rs):
//
//   * a room opened for the first time this session starts at its newest
//     message, once: visible, hidden, or empty until its first content lands;
//   * a room revisited in the same session comes back at the row the reader
//     left, with the rendered window (backfill depth) it had then;
//   * after any of those, arrivals (the reader's own sends included) and a
//     mobile hide and reveal keep the saved row at its gap.
//
// Every "does not move" assertion here fails against the old following
// implementation, which pinned a reader at the end, force-snapped on an own
// send, and reset every room switch to the newest 60 items at the bottom.
//
// Arrivals are INBOUND (`window.__riverTest`, example-data build) unless a test
// sends through the composer on purpose.

/// The fixture variant with rooms deeper than the render window: Deep History
/// Room has ~200 alternating-author messages (so three backfill steps of 60).
const DEEP_ROOM_PATH = "/?deep-history-room=1";
const DEEP_ROOM = "Deep History Room";
/// A second windowed room, for the room the reader goes to and comes back from.
const OTHER_ROOM = "Capped History Room";

/// Rendered history rows: display items plus date separators.
const HISTORY_ROWS = '[data-testid="conversation-history"] > *';

/// A row's gap may move this much and still be "in place" (whole-pixel reads,
/// fractional rows), far below a row's height.
const IN_PLACE_TOLERANCE_PX = 4;

/// How far above the end a parked reader sits: well past one arrival.
const PARK_PX = 1_500;

const ARRIVAL = (what: string) => `${what}: ${"v".repeat(200)}`;

const chat = (page: Page) => page.locator("#chat-scroll-container");

function renderedRowCount(page: Page): Promise<number> {
  return page.locator(HISTORY_ROWS).count();
}

/// DOM ids of every rendered message row, oldest first.
function renderedMessageIds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('#chat-scroll-container [id^="msg-"]')).map((row) => row.id),
  );
}

/// The newest rendered message row's DOM id.
async function newestRenderedId(page: Page): Promise<string> {
  const ids = await renderedMessageIds(page);
  expect(ids.length, "premise: the history should have messages").toBeGreaterThan(0);
  return ids[ids.length - 1];
}

/// `before`'s row is visible and back at its gap, polled, and stays so for five
/// samples over 500ms. Not "still the newest visible row": at the exact end a
/// row appended below can show its top 1-2px inside the view (the content's
/// bottom padding and the list's row gap nearly cancel), which is not the view
/// moving. Following is caught by the gap itself and by `expectNotFollowed`.
async function expectRowHolds(page: Page, before: RowPosition, why: string) {
  const drift = () => savedVisibleRowDrift(page, before);
  await expect.poll(drift, { timeout: 5_000, message: why }).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
  const drifts: number[] = [];
  for (let i = 0; i < 5; i++) {
    await page.waitForTimeout(100);
    drifts.push(await drift());
  }
  expect(Math.max(...drifts), `${why} (samples: ${drifts.join(", ")})`).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
}

/// The view is not at the end: something arrived below it and was not followed.
async function expectNotFollowed(page: Page, why: string) {
  expect(await distanceFromBottom(page), why).toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
}

/// The reader scrolls to `top` themselves; returns the newest message they see.
async function readerParksAt(page: Page, top: number): Promise<RowPosition> {
  await readerScrollsWithoutGesture(page, top);
  await afterLayoutSettles(page);
  const at = await newestVisibleRow(page);
  expect(at, "premise: a message should be visible where the reader parked").not.toBeNull();
  return at!;
}

/// Park `px` above the end of what is rendered.
async function readerParksAboveTheEnd(page: Page, px: number): Promise<RowPosition> {
  const top = (await historyHeight(page)) - (await viewportHeight(page)) - px;
  expect(top, "premise: the history should be tall enough to park in").toBeGreaterThan(0);
  const at = await readerParksAt(page, top);
  expect(await distanceFromBottom(page), "premise: the reader should be parked above the end").toBeGreaterThan(
    px / 2,
  );
  return at;
}

/// The reader reaches the top of what is rendered; one backfill step lands
/// above them. Returns the rendered row count after it.
async function backfillOnce(page: Page, why: string): Promise<number> {
  const before = await renderedRowCount(page);
  await page.evaluate(() => {
    document.getElementById("chat-scroll-container")!.scrollTop = 0;
  });
  await expect.poll(() => renderedRowCount(page), { timeout: 5_000, message: why }).toBeGreaterThan(before + 40);
  await afterLayoutSettles(page);
  return renderedRowCount(page);
}

/// Backfill the deep room once, then park with the view's bottom edge above
/// the oldest row the initial window rendered, so the saved row is one the
/// first render did not have.
async function parkInBackfilledHistory(page: Page): Promise<{ saved: RowPosition; rows: number }> {
  const initialIds = new Set(await renderedMessageIds(page));
  const oldHead = [...initialIds][0];
  await backfillOnce(page, "premise: reaching the top should backfill older rows");
  const top = await page.evaluate((id) => {
    const c = document.getElementById("chat-scroll-container")!;
    const row = document.getElementById(id)!;
    const inContent = row.getBoundingClientRect().top - c.getBoundingClientRect().top + c.scrollTop;
    return Math.floor(inContent - c.clientHeight - 200);
  }, oldHead);
  // Clear of BACKFILL_LEAD_PX (800) so parking does not page again.
  expect(top, "premise: the backfill should put a viewport of rows above the old head").toBeGreaterThan(1_000);
  const saved = await readerParksAt(page, top);
  expect(initialIds.has(saved.id), "premise: the saved row should be outside the initial window").toBe(false);
  return { saved, rows: await renderedRowCount(page) };
}

type Switcher = { by: string; to: (page: Page, room: string) => Promise<void> };

const SWITCHERS: Switcher[] = [
  { by: "the room list", to: selectListedRoom },
  {
    // As a notification click does.
    by: "a direct room switch",
    to: async (page, room) => {
      await callRiverTest(page, "switchRoom", room);
      await expect(page.getByRole("heading", { name: room })).toBeVisible({ timeout: 5_000 });
    },
  },
];

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
});

test.describe("The reader's own send does not move the view", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  // The room's own seeded history, not delivered fillers: a send goes through
  // the real `apply_delta`, and the `window.__riverTest` messages (unverified,
  // by authors who are not members) do not survive it. Measured: twelve
  // delivered fillers all vanished on one send, taking the reader's row with
  // them. The drafts are one line, so the composer does not grow and collapse
  // around the send.

  /// Send through the composer and wait until the message is on the page.
  async function sendOwn(page: Page, text: string) {
    await page.getByTestId("message-input").fill(text);
    await page.getByTestId("send-message-button").click();
    await expect(page.getByText(text)).toBeAttached({ timeout: 5_000 });
    await expect(page.getByTestId("message-input")).toHaveValue("");
  }

  test("sending while reading old messages keeps the reader's row", async ({ page }) => {
    await openRoomAtBottom(page, "Your Private Room");
    await expect(page.getByTestId("message-composer")).toBeVisible();
    const parked = await readerParksAboveTheEnd(page, 500);

    await sendOwn(page, "own send while parked");
    await expectRowHolds(page, parked, "the reader's own send moved the view off their row");
  });

  test("sending at the very end keeps the reader's row; the sent message stays below it", async ({ page }) => {
    await openRoomAtBottom(page, "Your Private Room");
    await expect(page.getByTestId("message-composer")).toBeVisible();
    await afterLayoutSettles(page);
    const atEnd = await newestVisibleRow(page);
    expect(atEnd?.id, "premise: the newest message is the one in view").toBe(await newestRenderedId(page));

    await sendOwn(page, "own send at the end");
    await expectRowHolds(page, atEnd!, "the reader's own send scrolled the view to it");
    await expectNotFollowed(page, "the sent message should be below the view");
  });
});

test.describe("Revisiting a room restores where the reader left it", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  for (const sw of SWITCHERS) {
    test(`A -> B -> A by ${sw.by}: A's backfilled row and window come back; B opens at its newest`, async ({
      page,
    }) => {
      await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
      const { saved, rows } = await parkInBackfilledHistory(page);

      await sw.to(page, OTHER_ROOM);
      await expectSettledAtBottom(page, "a room visited for the first time should open at its newest message");
      await afterLayoutSettles(page);
      expect((await newestVisibleRow(page))?.id, "B should open on its newest message").toBe(
        await newestRenderedId(page),
      );

      await sw.to(page, DEEP_ROOM);
      await expectRowHolds(page, saved, "coming back to A did not restore the row the reader left");
      expect(await renderedRowCount(page), "A's backfilled window should be rendered again").toBeGreaterThanOrEqual(
        rows,
      );

      // Then A behaves like any other position: an arrival is preserved...
      await deliver(page, ARRIVAL("arrival after coming back"));
      await expectRowHolds(page, saved, "an arrival after coming back moved the view");
      // ...and paging further back still works.
      await backfillOnce(page, "backfill after coming back revealed no older rows");
    });

    test(`each room keeps its own row, switching by ${sw.by}`, async ({ page }) => {
      await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
      const inA = await readerParksAboveTheEnd(page, PARK_PX);

      await sw.to(page, OTHER_ROOM);
      await expectSettledAtBottom(page, "B should open at its newest message, not at A's offset");
      const inB = await readerParksAboveTheEnd(page, PARK_PX * 2);
      expect(inB.id, "premise: the rooms show different rows").not.toBe(inA.id);

      await sw.to(page, DEEP_ROOM);
      await expectRowHolds(page, inA, "A did not come back at its own row");
      await sw.to(page, OTHER_ROOM);
      await expectRowHolds(page, inB, "B did not come back at its own row");
      await sw.to(page, DEEP_ROOM);
      await expectRowHolds(page, inA, "A did not come back at its own row a second time");

      // Paging in a revisited room that was never backfilled.
      await backfillOnce(page, "backfill after coming back revealed no older rows");
    });
  }

  // A guard rather than a regression: the old code also ignored a re-selection
  // (the window effect is keyed on an actual room change), so this passes there.
  test("re-selecting the room that is already open changes nothing", async ({ page }) => {
    await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
    const { saved, rows } = await parkInBackfilledHistory(page);

    await selectListedRoom(page, DEEP_ROOM);
    await afterLayoutSettles(page);
    await expectRowHolds(page, saved, "re-selecting the open room moved the view");
    expect(await renderedRowCount(page), "re-selecting the open room changed its window").toBe(rows);
  });
});

test.describe("A room opened for the first time starts at its newest message, once", () => {
  test.describe("while the chat is visible", () => {
    test.use({ viewport: { width: 1280, height: 900 } });

    test("it opens on its newest message, and the next arrival is preserved, not followed", async ({ page }) => {
      await openRoomAtBottom(page, DEEP_ROOM, DEEP_ROOM_PATH);
      await afterLayoutSettles(page);
      const opened = await newestVisibleRow(page);
      expect(opened?.id, "the room should open on its newest message").toBe(await newestRenderedId(page));

      await deliver(page, ARRIVAL("first arrival after opening"));
      await expectRowHolds(page, opened!, "the first arrival after opening moved the view");
      await expectNotFollowed(page, "the arrival should be below the view");
    });

    test("an empty room is placed at its newest message when its first content lands, then preserved", async ({
      page,
    }) => {
      await page.goto("/");
      await waitForApp(page);
      await page.getByTestId("create-room-button").click();
      await page.getByTestId("create-room-name-input").fill("Freshly Made Room");
      await page.getByTestId("create-room-nickname-input").fill("Alice");
      await page.getByTestId("create-room-submit-button").click();
      await expect(page.getByTestId("create-room-modal")).toHaveCount(0);
      await expect(page.getByRole("heading", { name: "Freshly Made Room" })).toBeVisible({ timeout: 5_000 });
      await expect(chat(page)).toBeVisible();
      expect(await renderedMessageIds(page), "premise: the new room starts empty").toEqual([]);

      // Its first content, in one update, taller than the view.
      await callRiverTest(page, "appendMessages", 40);
      await expect(page.getByText("batched arrival 39")).toBeAttached({ timeout: 5_000 });
      expect(
        (await historyHeight(page)) - (await viewportHeight(page)),
        "premise: the first content should overflow the view",
      ).toBeGreaterThan(500);
      await expectSettledAtBottom(page, "the room's first content should be placed at its newest message");
      await afterLayoutSettles(page);
      const placed = await newestVisibleRow(page);
      expect(placed?.id, "placed on the newest message").toBe(await newestRenderedId(page));

      await deliver(page, ARRIVAL("arrival after the first content"));
      await expectRowHolds(page, placed!, "an arrival after the first content moved the view");
      await expectNotFollowed(page, "the arrival should be below the view");
    });
  });

  test.describe("while the chat is hidden", () => {
    test.use({ viewport: { width: 390, height: 844 } });

    test("switched to while hidden, it is at its newest message on reveal, and the next arrival is preserved", async ({
      page,
    }) => {
      await openRoomAtBottom(page, "Team Chat Room", DEEP_ROOM_PATH);
      await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
      await expect(chat(page)).toBeHidden({ timeout: 5_000 });
      await afterLayoutSettles(page);

      await callRiverTest(page, "switchRoom", DEEP_ROOM);
      await expect
        .poll(() => renderedRowCount(page), {
          timeout: 5_000,
          message: "premise: the deep room should render while the chat is hidden",
        })
        .toBeGreaterThan(40);
      await afterLayoutSettles(page);

      await page.getByTestId("rooms-back-button").click();
      await expect(chat(page)).toBeVisible();
      await expect(page.getByRole("heading", { name: DEEP_ROOM })).toBeVisible();
      await expectSettledAtBottom(page, "the room opened while hidden should be at its newest message on reveal");
      await afterLayoutSettles(page);
      const opened = await newestVisibleRow(page);
      expect(opened?.id, "revealed on the newest message").toBe(await newestRenderedId(page));

      await deliver(page, ARRIVAL("first arrival after the reveal"));
      await expectRowHolds(page, opened!, "the first arrival after the reveal moved the view");
      await expectNotFollowed(page, "the arrival should be below the view");
    });
  });
});

test.describe("Mobile panels hide the chat at the end; arrivals while hidden", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  for (const panel of [
    { name: "Rooms", opener: "hamburger-rooms-button", back: "rooms-back-button" },
    { name: "Members", opener: "header-members-button", back: "members-back-button" },
  ] as const) {
    test(`the ${panel.name} panel: the reveal shows the row the reader left, not the new end`, async ({ page }) => {
      await openRoomAtBottom(page, "Team Chat Room");
      await fillHistory(page);
      await afterLayoutSettles(page);
      const atEnd = await newestVisibleRow(page);
      expect(atEnd?.id, "premise: the newest message is the one in view").toBe(await newestRenderedId(page));

      await page.getByTestId(panel.opener).filter({ visible: true }).click();
      await expect(chat(page)).toBeHidden({ timeout: 5_000 });
      await expect.poll(() => viewportHeight(page), { message: "premise: the hidden chat has no height" }).toBe(0);
      await afterLayoutSettles(page);
      for (let i = 0; i < 3; i++) {
        const text = ARRIVAL(`hidden arrival ${i}`);
        await callRiverTest(page, "appendMessage", text);
        await expect(page.getByText(text)).toBeAttached({ timeout: 5_000 });
      }
      await afterLayoutSettles(page);

      await page.getByTestId(panel.back).click();
      await expect(chat(page)).toBeVisible();
      await afterLayoutSettles(page);
      await expectRowHolds(page, atEnd!, "the reveal did not show the row the reader left");
      await expectNotFollowed(page, "the arrivals while hidden should be below the view");
    });
  }
});
