import { test, expect, Locator, Page } from "@playwright/test";
import {
  BACKDROP,
  centreOf,
  chips,
  choices,
  coarsePointer,
  committedRow,
  controlStates,
  expectSheetOpenFor,
  hasTouch,
  historyPoint,
  keyboardFocus,
  narrowLayout,
  openSheetFromRow,
  openRoomWithComposer,
  openRoomWithTarget,
  openSheetFromReact,
  palette,
  PANEL,
  plainPointOf,
  press,
  pressAt,
  rowKey,
  settle,
  SHEET,
  sheet,
  SHOWN,
  submitMessage,
  tapRow,
  touchmovePrevented,
  wheelCancelled,
} from "./example-room";
import { deliver, scrollMetrics } from "./history-geometry";
import { callRiverTest, rekeyThenClick } from "./river-test";

// Below Tailwind's `md` (48rem) a signed message's trailing Reply, Edit and
// Delete are hidden; a tap on the row opens the modal message sheet
// (mobile_message_sheet.rs). Its one reaction + stays visible and opens the
// same sheet. On top, the 16 choices the desktop picker has, in one row that
// scrolls sideways: a tap on one reacts and closes it. Then who reacted,
// then those actions. It has no field, so it never asks for the software
// keyboard; a tap outside dismisses it. A reaction
// chip opens nothing: a press on it reacts with its emoji
// (reaction-owners-touch.spec.ts); the sheet's drags are in
// mobile-message-sheet-gestures.spec.ts.
// Width alone decides: a narrow mouse window is mobile, a wide touch screen
// keeps the desktop footer. These run on every project at the widths they
// set, so the desktop projects are narrow mouse windows here.

const FREQUENT = ["👍", "❤️", "😂", "👀", "🔥", "😮", "😢", "😡", "🎉", "🤔", "👋", "✅", "💯", "🙏", "😍", "👏"];
/// The first choice, for a click queued inside one `page.evaluate`.
const FIRST_CHOICE = `#${SHEET} .mobile-reaction-choice`;

/// The row's sideways scroll and how far it can go.
function rowScroll(page: Page): Promise<{ left: number; max: number }> {
  return palette(page).evaluate((el) => ({ left: el.scrollLeft, max: el.scrollWidth - el.clientWidth }));
}

/// Whether `choice` lies wholly inside the row's visible box.
async function inRowView(page: Page, choice: Locator): Promise<boolean> {
  const [c, p] = [(await choice.boundingBox())!, (await palette(page).boundingBox())!];
  return c.x >= p.x - 1 && c.x + c.width <= p.x + p.width + 1;
}

/// Whether nothing opened: no sheet, and no tap reached one.
async function expectNoSheet(page: Page) {
  await settle(page);
  await expect(sheet(page)).toBeHidden();
}

/// Every control a Tab press can reach in the document, by test id.
function tabbableIds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll<HTMLElement>("button, a[href], input, textarea, select, [tabindex]"))
      .filter((e) => e.tabIndex >= 0 && !(e as HTMLButtonElement).disabled && e.getClientRects().length > 0)
      .map((e) => e.getAttribute("data-testid") ?? ""),
  );
}

/// A received message and an own one, neither with reactions; the own one
/// followed by a continuation of it.
async function openRoomWithRows(page: Page) {
  await openRoomWithComposer(page);
  await callRiverTest(page, "insertMessageBeforeLast", "received row");
  const own = await submitMessage(page, "own first row");
  await expect(own).toHaveCount(1);
  const continuation = await submitMessage(page, "own continuation row");
  await expect(continuation).toHaveCount(1);
  await page.getByTestId("message-input").blur();
  return { received: committedRow(page, "received row"), own, continuation };
}

test.describe("Below 48rem", () => {
  test.use({ viewport: { width: 390, height: 760 } });

  test("Reply, Edit and Delete leave the footer, its hits and the tab order; one + per row stays visible", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    expect(await narrowLayout(page), "premise: below 48rem").toBe(true);
    await own.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await page.mouse.move(0, 0);
    expect(await controlStates(own, ["message-react-button"]), "the + shows at rest").toEqual({ "message-react-button": SHOWN });
    expect(await own.getByTestId("message-action-cluster").evaluate((el) => getComputedStyle(el).display)).toBe("none");
    // A row takes no double-tap zoom: iOS Safari holds a tap's click back
    // while a double tap could still zoom.
    expect(await own.evaluate((el) => getComputedStyle(el).touchAction)).toBe("manipulation");
    const tabbable = await tabbableIds(page);
    for (const id of ["message-reply-button", "message-edit-button", "message-delete-button"]) {
      expect(tabbable, `${id} is no tab stop`).not.toContain(id);
    }
    expect(tabbable, "the + stays a tab stop").toContain("message-react-button");
    expect(tabbable, "and so does each row's actions entry").toContain("mobile-message-actions-entry");
    // Every signed row: exactly one way to react and one way into actions.
    const perRow = await page.getByTestId("message-row").evaluateAll((rows) =>
      rows.map((r) => [
        r.querySelectorAll('[data-testid="message-react-button"]').length,
        r.querySelectorAll('[data-testid="mobile-message-actions-entry"]').length,
      ]),
    );
    expect(perRow.length).toBeGreaterThan(3);
    for (const counts of perRow) expect(counts).toEqual([1, 1]);
  });

  test("a tap on a row opens the sheet for that exact message; a tap outside closes it and reaches nothing underneath", async ({ page }) => {
    const { received, own, continuation } = await openRoomWithRows(page);
    const s = sheet(page);
    await settle(page);
    const before = (await scrollMetrics(page)).top;
    // Self owns this room: owning it grants no Edit or Delete on another
    // author's message, here or in the footer.
    const rows = [
      [received, ["reply"]],
      [own, ["reply", "edit", "delete"]],
      [continuation, ["reply", "edit", "delete"]],
    ] as const;
    for (const [row, expected] of rows) {
      await openSheetFromRow(page, row);
      const actions = await s.locator(".mobile-sheet-actions button").evaluateAll((bs) =>
        bs.map((b) => b.getAttribute("data-testid")!.replace("mobile-message-action-", "")),
      );
      expect(actions).toEqual(expected);
      if (row === continuation) break;
      await page.keyboard.press("Escape");
      await expect(s).toBeHidden();
    }
    // On the one left open: the emoji row above the actions.
    const [paletteBox, actionsBox] = [await palette(page).boundingBox(), await s.locator(".mobile-sheet-actions").boundingBox()];
    expect(paletteBox!.y, "emoji first, actions below").toBeLessThan(actionsBox!.y);
    await expect(continuation.getByTestId("message-react-button")).toHaveAttribute("aria-expanded", "true");
    await expect(continuation.getByTestId("mobile-message-actions-entry")).toHaveAttribute("aria-expanded", "true");
    // Not the keyboard: an emoji (a button) has focus.
    await expect(s.locator(".mobile-reaction-palette button:focus")).toHaveCount(1);
    // A tap outside, over the header's rooms button: it lands on the
    // backdrop and must not open the rooms panel.
    const [x, y] = await centreOf(page.getByTestId("hamburger-rooms-button").filter({ visible: true }).first());
    const point = { x, y };
    const hit = await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid") ?? null, point);
    expect(hit, "premise: the backdrop covers the header").toBe(BACKDROP);
    await pressAt(page, point);
    await expect(s).toBeHidden();
    await settle(page);
    await expect(page.locator("#chat-scroll-container"), "the chat is still the panel shown").toBeVisible();
    await expect(page.getByTestId("room-list").filter({ visible: true }), "the rooms panel did not open").toHaveCount(0);
    // Focus is back on the row's entry, and nothing scrolled.
    await expect(continuation.getByTestId("mobile-message-actions-entry")).toBeFocused();
    await expect(continuation.getByTestId("mobile-message-actions-entry")).toHaveAttribute("aria-expanded", "false");
    expect((await scrollMetrics(page)).top, "opening and dismissing scrolled nothing").toBe(before);
  });

  test("a link, a reply strip and the author's name and avatar keep their own behaviour and open nothing", async ({ page, context }) => {
    await openRoomWithComposer(page);
    await deliver(page, "nested link https://freenet.org/ inside");
    const linkRow = committedRow(page, "nested link");
    const popups: Page[] = [];
    context.on("page", (p) => popups.push(p));
    await linkRow.locator("a[href]").evaluate((a) => a.addEventListener("click", (e) => e.preventDefault()));
    await press(linkRow.locator("a[href]"));
    await expectNoSheet(page);
    for (const p of popups) await p.close();

    const quoted = page.getByTestId("message-row").filter({ has: page.getByTestId("reply-strip") }).first();
    await quoted.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await press(quoted.getByTestId("reply-strip"));
    await expectNoSheet(page);

    const first = page.getByTestId("message-row").filter({ has: page.getByTestId("message-author-name") }).last();
    await first.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await press(first.getByTestId("message-author-name"));
    await expect(page.getByTestId("member-info-modal")).toBeVisible();
    await expect(sheet(page)).toBeHidden();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("member-info-modal")).toBeHidden();
    await press(first.getByTestId("message-author-avatar"));
    await expect(page.getByTestId("member-info-modal")).toBeVisible();
    await expect(sheet(page)).toBeHidden();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("member-info-modal")).toBeHidden();
  });

  test("a press that moved, a text selection and a modified click open nothing; a plain tap after them does", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    const { x, y } = await plainPointOf(own);
    // A press dragged past a tap's slop, released on the row.
    if (hasTouch()) {
      await own.dispatchEvent("pointerdown", { pointerId: 7, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
      await own.dispatchEvent("pointerup", { pointerId: 7, pointerType: "touch", isPrimary: true, clientX: x, clientY: y + 30, bubbles: true });
      await own.dispatchEvent("click", { clientX: x, clientY: y + 30, button: 0, bubbles: true });
    } else {
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.move(x, y - 30, { steps: 4 });
      await page.mouse.up();
    }
    await expectNoSheet(page);
    // A press the browser cancelled (it became a scroll).
    await own.dispatchEvent("pointerdown", { pointerId: 8, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("pointercancel", { pointerId: 8, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expectNoSheet(page);
    // A second finger joining it: a pinch.
    await own.dispatchEvent("pointerdown", { pointerId: 9, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerdown", { pointerId: 10, pointerType: "touch", isPrimary: false, clientX: x + 40, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerup", { pointerId: 10, pointerType: "touch", isPrimary: false, clientX: x + 40, clientY: y, bubbles: true });
    await own.dispatchEvent("pointerup", { pointerId: 9, pointerType: "touch", isPrimary: true, clientX: x, clientY: y, bubbles: true });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expectNoSheet(page);
    // A tap completing a selection in the row.
    await own.getByTestId("message-body").evaluate((el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      const s = window.getSelection()!;
      s.removeAllRanges();
      s.addRange(range);
    });
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, bubbles: true });
    await expectNoSheet(page);
    expect(await page.evaluate(() => window.getSelection()!.toString()), "the selection is left alone").toContain("own first row");
    await page.evaluate(() => window.getSelection()!.removeAllRanges());
    await own.dispatchEvent("click", { clientX: x, clientY: y, button: 0, ctrlKey: true, bubbles: true });
    await expectNoSheet(page);
    // And then a plain tap does.
    await tapRow(page, own);
    await expect(sheet(page)).toBeVisible();
  });

  test("the keyboard has its own entry: shown on focus, Enter opens, Tab stays inside, Escape returns to it", async ({ page, browserName }) => {
    const tab = browserName === "webkit" ? "Alt+Tab" : "Tab";
    const backTab = browserName === "webkit" ? "Alt+Shift+Tab" : "Shift+Tab";
    const { own } = await openRoomWithRows(page);
    const entry = own.getByTestId("mobile-message-actions-entry");
    await expect(entry).toHaveAccessibleName("Message options");
    await expect(entry).toHaveAttribute("aria-haspopup", "dialog");
    await expect(entry).toHaveAttribute("aria-controls", SHEET);
    await keyboardFocus(page, entry);
    const box = (await entry.boundingBox())!;
    expect(box.width, "keyboard focus shows it").toBeGreaterThan(20);
    await page.keyboard.press("Enter");
    await expectSheetOpenFor(page, own);
    const first = choices(page).first();
    const del = page.getByTestId("mobile-message-action-delete");
    await expect(first, "the first emoji takes focus").toBeFocused();
    // Tab order: the 16 emoji, then Reply, Edit, Delete; wrapping both ways.
    await del.focus();
    await page.keyboard.press(tab);
    await expect(first, "Tab from the last control wraps to the first").toBeFocused();
    await page.keyboard.press(backTab);
    await expect(del, "Shift+Tab from the first wraps to the last").toBeFocused();
    expect(await page.locator("dialog[open]").count()).toBe(1);
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await expect(entry, "focus returns to the entry").toBeFocused();
    await expect(chips(own), "Escape reacts with nothing").toHaveCount(0);
  });

  test("Reply, Edit and Delete act on the exact message, closing first", async ({ page }) => {
    const { own, continuation } = await openRoomWithRows(page);
    const composer = page.getByTestId("message-composer");

    await openSheetFromRow(page, continuation);
    await press(page.getByTestId("mobile-message-action-reply"));
    await expect(sheet(page)).toBeHidden();
    await expect(composer).toContainText("Replying to");
    await expect(composer).toContainText("own continuation row");
    await expect(page.getByTestId("message-input"), "the reply hands focus to the composer").toBeFocused();
    await page.getByTitle("Cancel reply").click();

    const ownKey = await rowKey(own);
    const ownRow = page.locator(`[id="msg-${ownKey}"]`);
    await openSheetFromRow(page, own);
    await press(page.getByTestId("mobile-message-action-edit"));
    await expect(sheet(page)).toBeHidden();
    const editArea = page.locator(`textarea[id="edit-msg-${ownKey}"]`);
    await expect(editArea, "the editor opens on that row").toBeVisible();
    await expect(editArea).toHaveValue("own first row");
    await expect(editArea).toBeFocused();
    await expect(ownRow.getByTestId("message-react-button"), "no + on a row being edited").toHaveCount(0);
    await expect(ownRow.getByTestId("mobile-message-actions-entry")).toHaveCount(0);
    // A tap on the row being edited opens nothing.
    await ownRow.locator(".msg-gutter").first().dispatchEvent("click", { button: 0, bubbles: true });
    await expectNoSheet(page);
    await press(ownRow.getByTestId("edit-cancel-button"));
    await expect(editArea).toHaveCount(0);

    await openSheetFromRow(page, continuation);
    await press(page.getByTestId("mobile-message-action-delete"));
    const heading = page.getByRole("heading", { name: /Delete Message/ });
    await expect(heading).toBeVisible();
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(heading).toHaveCount(0);
    await expect(continuation).toHaveCount(1);
    await openSheetFromRow(page, continuation);
    await press(page.getByTestId("mobile-message-action-delete"));
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(continuation).toHaveCount(0);
    await expect(own, "only that message").toHaveCount(1);
  });

  test("its message leaving closes it for good; a remount keeps it; a room switch closes it", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    // Remount: removing a group's first message re-keys the group.
    await callRiverTest(page, "deliverTimedMessages", 0, [
      { text: "sheet lead", sentSecs: 1, receivedSecs: 0 },
      { text: "sheet second", sentSecs: 2, receivedSecs: 0 },
      { text: "sheet survivor", sentSecs: 3, receivedSecs: 0 },
    ]);
    const survivor = committedRow(page, "sheet survivor");
    await expect(survivor).toHaveCount(1);
    const remountUnderSheet = async (lead: string) => {
      await openSheetFromRow(page, survivor);
      await survivor.evaluate((el) => ((window as unknown as { __old: Element }).__old = el));
      await callRiverTest(page, "removeMessages", lead);
      await expect(committedRow(page, lead)).toHaveCount(0);
      await settle(page);
      expect(await survivor.evaluate((el) => el === (window as unknown as { __old: Element }).__old), "premise: remounted").toBe(false);
      await expect(sheet(page), "a remount keeps it").toBeVisible();
    };
    await remountUnderSheet("sheet lead");
    await page.keyboard.press("Escape");
    await expect(survivor.getByTestId("mobile-message-actions-entry"), "focus goes to the new entry").toBeFocused();
    // A choice in a sheet that outlived a remount still acts.
    await remountUnderSheet("sheet second");
    await press(choices(page).first());
    await expect(sheet(page)).toBeHidden();
    await expect(chips(survivor), "and a choice acts on it, once").toHaveCount(1);
    await expect(survivor.getByTestId("mobile-message-actions-entry"), "focus goes to the new entry").toBeFocused();

    // Deleted while open (optimistically, signature held), then rolled back.
    const own = await submitMessage(page, "sheet doomed");
    await expect(own).toHaveCount(1);
    await page.getByTestId("message-input").blur();
    await callRiverTest(page, "holdMessageSigning");
    await openSheetFromRow(page, own);
    await press(page.getByTestId("mobile-message-action-delete"));
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(own).toHaveCount(0);
    await openSheetFromRow(page, committedRow(page, "sheet survivor"));
    await callRiverTest(page, "removeMessages", "sheet survivor");
    await expect(sheet(page), "its message left: it closes").toBeHidden();
    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(own, "rolled back").toHaveCount(1);
    await settle(page);
    await expect(sheet(page), "a rollback never reopens it").toBeHidden();

    // A room switch.
    await openSheetFromRow(page, own);
    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await expect(sheet(page)).toBeHidden();
    await callRiverTest(page, "switchRoom", "Your Private Room");
    await expect(own).toHaveCount(1);
    await settle(page);
    await expect(sheet(page)).toBeHidden();
  });

  test("while open the background holds still, arrivals follow, and a reload waits; closing lets go", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openSheetFromRow(page, own);
    expect(await callRiverTest(page, "userIsBusy"), "busy while open").toBe(true);
    const over = await historyPoint(page);
    if (!hasTouch()) {
      const before = (await scrollMetrics(page)).top;
      expect(before, "premise: the history has room to scroll up").toBeGreaterThan(300);
      await page.mouse.move(over.x, over.y);
      await page.mouse.wheel(0, -300);
      await settle(page);
      expect((await scrollMetrics(page)).top, "the history did not move").toBe(before);
      // A zoom is let through: not cancelled.
      expect(await wheelCancelled(page, over.x, over.y, true), "Ctrl+wheel is a zoom").toBe(false);
    }
    // An arrival is still followed by a reader at the end.
    await callRiverTest(page, "appendMessage", "arrives under the sheet");
    await expect(committedRow(page, "arrives under the sheet")).toBeInViewport();
    await expect(sheet(page)).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "closed: not busy" }).toBe(false);
    if (!hasTouch()) {
      const before = (await scrollMetrics(page)).top;
      await page.mouse.move(over.x, over.y);
      await page.mouse.wheel(0, -300);
      await expect.poll(async () => (await scrollMetrics(page)).top, { message: "closed: the wheel scrolls again" }).toBeLessThan(before);
    }
  });

  test("one-finger touch is held outside the panel, not inside it or for a pinch; every close path lets go", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    const closes: [string, () => Promise<unknown>][] = [
      ["Escape", () => page.keyboard.press("Escape")],
      ["backdrop", async () => pressAt(page, await historyPoint(page))],
      ["a choice", () => press(choices(page).first())],
      ["48rem", async () => {
        await page.setViewportSize({ width: 768, height: 760 });
        await expect(sheet(page)).toBeHidden();
        await page.setViewportSize({ width: 390, height: 760 });
      }],
      // Last: it takes the row away.
      ["its message leaving", () => callRiverTest(page, "removeMessages", "own first row")],
    ];
    for (const [how, close] of closes) {
      await openSheetFromRow(page, own);
      const h = await historyPoint(page);
      expect(await touchmovePrevented(page, h.x, h.y), `${how}: a finger over the history is held`).toBe(true);
      expect(await touchmovePrevented(page, h.x, h.y, 2), `${how}: a pinch is not`).toBe(false);
      // Inside the panel, on no control.
      const panel = (await page.locator(PANEL).boundingBox())!;
      const inside = { x: panel.x + panel.width / 2, y: panel.y + 6 };
      expect(await touchmovePrevented(page, inside.x, inside.y), `${how}: the panel scrolls`).toBe(false);
      expect(await touchmovePrevented(page, inside.x, inside.y, 2), `${how}: and a pinch on it zooms`).toBe(false);
      await close();
      await expect(sheet(page)).toBeHidden();
      await settle(page);
      const after = await historyPoint(page);
      expect(await page.evaluate(({ x, y }) => !!document.elementFromPoint(x, y)?.closest("#chat-scroll-container"), after), "premise: the history is under the point").toBe(true);
      expect(await touchmovePrevented(page, after.x, after.y), `closed by ${how}: let go`).toBe(false);
    }
  });

  test("an arrival carrying its row out of view keeps it open, usable and holding", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openSheetFromRow(page, own);
    const tall = Array.from({ length: 40 }, (_, i) => `tall arrival ${i}`).join("\n\n");
    await callRiverTest(page, "appendMessage", tall);
    await expect(page.getByTestId("message-row").filter({ hasText: "tall arrival 39" })).toHaveCount(1);
    await settle(page);
    const ownBox = await own.evaluate((el) => {
      const r = el.getBoundingClientRect();
      const v = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      return { above: r.bottom <= v.top, connected: el.isConnected };
    });
    expect(ownBox, "premise: still rendered, out of the history's view").toEqual({ above: true, connected: true });
    await expect(sheet(page), "still open").toBeVisible();
    const h = await historyPoint(page);
    expect(await touchmovePrevented(page, h.x, h.y), "still holding the background").toBe(true);
    // Dismissed, focus returns to the row's entry without scrolling to it.
    const entry = own.getByTestId("mobile-message-actions-entry");
    const top = (await scrollMetrics(page)).top;
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await expect(entry).toBeFocused();
    await settle(page);
    expect((await scrollMetrics(page)).top, "focus came back without scrolling").toBe(top);
    // And opened again from there, it acts on that message.
    await page.keyboard.press("Enter");
    await expectSheetOpenFor(page, own);
    await press(page.getByTestId("mobile-message-action-reply"));
    await expect(page.getByTestId("message-composer")).toContainText("own first row");
  });

  test("leaving the chat panel or a re-key of the room closes it, and a queued action then does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await openSheetFromRow(page, own);
    await callRiverTest(page, "setMobileView", "members");
    await expect(sheet(page)).toBeHidden();
    await callRiverTest(page, "setMobileView", "chat");
    await settle(page);
    await expect(sheet(page), "coming back reopens nothing").toBeHidden();

    await openSheetFromRow(page, own);
    await rekeyThenClick(page, '[data-testid="mobile-message-action-reply"]');
    await settle(page);
    await expect(sheet(page)).toBeHidden();
    await expect(page.getByTestId("message-composer"), "the Reply queued across the re-key did nothing").not.toContainText("Replying to");
    // In the new generation a fresh opening works.
    await openSheetFromRow(page, own);
  });

  test("opening and closing it renders no message group", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    await settle(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await openSheetFromRow(page, own);
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    expect(await callRiverTest(page, "takeMessageGroupRenders")).toBe(0);
  });
});

test.describe("Reacting from the message sheet", () => {
  test.use({ viewport: { width: 390, height: 760 } });

  test("+ opens one row of the 16 choices, the first focused, with no field and the composer untouched", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    // A draft and its selection in the composer.
    const composer = page.getByTestId("message-input");
    await composer.fill("my unsent draft");
    await composer.evaluate((el: HTMLTextAreaElement) => el.setSelectionRange(3, 9));
    const react = row.getByTestId("message-react-button");
    await expect(react).toHaveAttribute("aria-haspopup", "dialog");
    await expect(react).toHaveAttribute("aria-controls", SHEET);
    await expect(react).toHaveAttribute("aria-expanded", "false");
    await openSheetFromReact(page, row);
    await expect(react).toHaveAttribute("aria-expanded", "true");
    await expect(sheet(page), "named, though it shows no title").toHaveAccessibleName("Message options");
    await expect(sheet(page).getByTestId("mobile-message-action-reply"), "the actions below the emoji").toBeVisible();
    await expect(sheet(page).getByTestId("mobile-sheet-reactors"), "no reactions, no list").toHaveCount(0);
    await expect(sheet(page).locator("input, textarea, [contenteditable]"), "no field to raise a keyboard").toHaveCount(0);
    await expect(palette(page)).toHaveAttribute("role", "group");
    await expect(palette(page)).toHaveAccessibleName("Reactions");
    expect(await choices(page).allTextContents(), "every one of the existing 16, in order").toEqual(FREQUENT);
    await expect(choices(page).first()).toHaveAccessibleName("React with 👍");
    await expect(choices(page).first(), "the first choice takes focus").toBeFocused();
    const g = await palette(page).evaluate((el) => ({
      tops: new Set(Array.from(el.children).map((c) => Math.round(c.getBoundingClientRect().top))).size,
      scrolls: el.scrollWidth > el.clientWidth,
      left: el.scrollLeft,
    }));
    expect(g.tops, "one row").toBe(1);
    expect(g.scrolls, "wider than the panel: it scrolls sideways").toBe(true);
    expect(g.left, "it opens at the start").toBe(0);
    // A tap outside, over the history, lands on the backdrop and dismisses it.
    const outside = await historyPoint(page);
    expect(await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid"), outside)).toBe(BACKDROP);
    await pressAt(page, outside);
    await expect(sheet(page)).toBeHidden();
    await expect(react, "focus back on the +").toBeFocused();
    expect(await composer.evaluate((el: HTMLTextAreaElement) => [el.value, el.selectionStart, el.selectionEnd])).toEqual(["my unsent draft", 3, 9]);
    await expect(chips(row), "dismissing reacts with nothing").toHaveCount(0);
  });

  test("a tap reacts and closes it; the same emoji removes, another switches", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    await press(choices(page).first());
    await expect(sheet(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("👍");
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();

    // The current one is marked, and a tap on it removes it.
    await openSheetFromReact(page, row);
    await press(palette(page).getByRole("button", { name: "Remove 👍 reaction" }));
    await expect(chips(row)).toHaveCount(0);

    // Another switches: one chip, the new emoji.
    await openSheetFromReact(page, row);
    await press(palette(page).getByRole("button", { name: "React with 🎉" }));
    await expect(chips(row)).toHaveCount(1);
    await openSheetFromReact(page, row);
    await press(palette(page).getByRole("button", { name: "React with 😂" }));
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("😂");
  });

  test("the current reaction is scrolled into the row's view and focused when it reopens", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    const last = palette(page).getByRole("button", { name: "React with 👏" });
    expect(await inRowView(page, last), "premise: the last choice starts out of view").toBe(false);
    await press(last);
    await expect(chips(row)).toContainText("👏");
    await openSheetFromReact(page, row);
    const current = palette(page).getByRole("button", { name: "Remove 👏 reaction" });
    await expect(current, "the current reaction takes focus").toBeFocused();
    expect(await inRowView(page, current), "scrolled into the row's view").toBe(true);
    const { left, max } = await rowScroll(page);
    expect(left, "the last one: the row at its end").toBeGreaterThan(max - 2);
  });

  test("a duplicate tap acts once and does not undo itself", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await page.evaluate((sel) => {
      const choice = document.querySelector<HTMLElement>(sel)!;
      choice.click();
      choice.click();
    }, FIRST_CHOICE);
    await settle(page);
    await expect(chips(row), "one reaction, not toggled back off").toHaveCount(1);
    await expect(chips(row)).toContainText("👍");
    await expect.poll(() => callRiverTest(page, "messageSignsRecorded"), { message: "signed once" }).toBe(signs + 1);
  });

  test("a tap queued before a dismissal and reopening of the same message does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    const key = await rowKey(row);
    // In one task: a tap outside, + again on the same message, then the old
    // choice.
    await page.evaluate(
      ({ key, sel, backdrop }) => {
        const choice = document.querySelector<HTMLElement>(sel)!;
        const close = document.querySelector<HTMLElement>(`[data-testid="${backdrop}"]`)!;
        const plus = document.querySelector<HTMLElement>(`[id="msg-${key}"] [data-testid="message-react-button"]`)!;
        close.click();
        plus.click();
        choice.click();
      },
      { key, sel: FIRST_CHOICE, backdrop: BACKDROP },
    );
    await settle(page);
    await expect(chips(row), "the earlier opening's tap did nothing").toHaveCount(0);
    await expect(sheet(page), "the new opening is open").toBeVisible();
  });

  test("the sheet lists who reacted, between the emoji and the actions: a member by name, the reader as You", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    await expect(chips(row)).toContainText("1");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));

    // The member, not the reader.
    await openSheetFromReact(page, row);
    const list = sheet(page).getByTestId("mobile-sheet-reactors");
    await expect(list).toHaveAccessibleName("Who reacted");
    await expect(list.getByTestId("mobile-sheet-reaction")).toHaveCount(1);
    await expect(list.getByTestId("mobile-sheet-reaction")).toContainText("🎉");
    const names = await list.getByTestId("mobile-sheet-reactor").allTextContents();
    expect(names, "the member who reacted, not the reader").toHaveLength(1);
    expect(names[0]).not.toBe("You");
    const [paletteBox, listBox, actionsBox] = [
      await palette(page).boundingBox(),
      await list.boundingBox(),
      await sheet(page).locator(".mobile-sheet-actions").boundingBox(),
    ];
    expect(paletteBox!.y, "emoji, then who reacted").toBeLessThan(listBox!.y);
    expect(listBox!.y, "then the actions").toBeLessThan(actionsBox!.y);
    await expect(choices(page).first(), "an emoji takes focus").toBeFocused();

    // Once the reader reacts too, it names them as "You".
    await press(palette(page).getByRole("button", { name: "React with 🎉" }));
    await expect(chips(row)).toContainText("2");
    await openSheetFromReact(page, row);
    await expect(list.getByTestId("mobile-sheet-reactor")).toHaveCount(2);
    await expect(list.getByTestId("mobile-sheet-reactor").filter({ hasText: /^You$/ })).toHaveCount(1);
    await expect(palette(page).getByRole("button", { name: "Remove 🎉 reaction" }), "and it is their current one").toBeVisible();
  });

  test("opened by a tap its focused emoji shows no ring until a key is pressed; opened by a key it shows at once", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    const outline = () => page.evaluate(() => getComputedStyle(document.activeElement!).outlineStyle);
    await openSheetFromReact(page, row);
    await expect(choices(page).first()).toBeFocused();
    expect(await outline(), "no ring that reads as the current reaction").toBe("none");
    // Any key in it: the reader is on the keyboard now. Read from the
    // attribute the rule keys on: Firefox does not turn a lone modifier into
    // `:focus-visible`, so the ring itself is not the same on every engine.
    await page.keyboard.press("Shift");
    await expect(sheet(page)).toHaveAttribute("data-opened-by", "keyboard");
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();

    // From the keyboard: the ring shows straight away.
    await keyboardFocus(page, row.getByTestId("message-react-button"));
    await page.keyboard.press("Enter");
    await expect(sheet(page)).toBeVisible();
    await expect(choices(page).first()).toBeFocused();
    expect(await outline(), "the keyboard's focus ring").not.toBe("none");
  });

  test("without the Popover API the + still opens it", { tag: "@chromium-only" }, async ({ page }) => {
    await page.addInitScript(() => {
      Object.defineProperty(HTMLElement.prototype, "showPopover", { value: undefined, configurable: true, writable: true });
    });
    const row = await openRoomWithTarget(page);
    const react = row.getByTestId("message-react-button");
    await expect(react).not.toHaveAttribute("aria-disabled");
    await openSheetFromReact(page, row);
    await press(choices(page).first());
    await expect(chips(row)).toHaveCount(1);
  });

  test("a sideways wheel over the row scrolls it", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    const before = (await scrollMetrics(page)).top;
    await palette(page).hover();
    await page.mouse.wheel(200, 0);
    await expect.poll(async () => (await rowScroll(page)).left, { message: "the row scrolled" }).toBeGreaterThan(0);
    expect((await scrollMetrics(page)).top, "the history did not move").toBe(before);
    await expect(sheet(page)).toBeVisible();
  });

  test("a reaction from the sheet that cannot be signed is rolled back, and says so", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "holdMessageSigning");
    await openSheetFromReact(page, row);
    await press(choices(page).first());
    await expect(chips(row), "shown while its signature is held").toHaveCount(1);
    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(chips(row), "rolled back").toHaveCount(0);
    await expect(page.getByTestId("toast-message")).toHaveText("Your reaction could not be sent");
    await expect(sheet(page), "nothing reopens").toBeHidden();
  });
});

test.describe("The message sheet on a small screen", () => {
  test.use({ viewport: { width: 320, height: 360 } });

  test("fits at 320px on the bottom edge, every target at least 44px, in one row whose every choice is reachable", async ({ page }) => {
    // An own message: the sheet at its tallest, with Reply, Edit and Delete.
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "own small-screen row");
    await expect(row).toHaveCount(1);
    await page.getByTestId("message-input").blur();
    await openSheetFromReact(page, row);
    const g = await page.locator(PANEL).evaluate((panel) => {
      const r = panel.getBoundingClientRect();
      const palette = panel.querySelector(".mobile-reaction-palette")!.getBoundingClientRect();
      const vw = document.documentElement.clientWidth;
      return {
        vw,
        vh: window.innerHeight,
        top: r.top,
        bottom: r.bottom,
        left: r.left,
        right: r.right,
        rowHeight: palette.height,
        overflowX: document.documentElement.scrollWidth > vw,
        actions: panel.querySelectorAll(".mobile-sheet-actions button").length,
        // Under 44px either way, or past the screen's edge (an emoji past
        // the row's edge is reached by scrolling the row).
        bad: Array.from(panel.querySelectorAll("button"))
          .filter((b) => {
            const br = b.getBoundingClientRect();
            const right = b.closest(".mobile-reaction-palette") ? Math.min(br.right, palette.right) : br.right;
            return br.height < 44 || br.width < 44 || right > vw;
          })
          .map((b) => b.getAttribute("data-testid") ?? b.textContent),
      };
    });
    expect(g.actions, "premise: Reply, Edit and Delete").toBe(3);
    expect(g.overflowX, "nothing overflows the page").toBe(false);
    expect(g.top, "inside the viewport").toBeGreaterThanOrEqual(0);
    expect(Math.abs(g.bottom - g.vh), "on the bottom edge").toBeLessThan(2);
    expect(g.left).toBeGreaterThanOrEqual(0);
    expect(g.right).toBeLessThanOrEqual(g.vw);
    expect(g.rowHeight, "one row").toBeLessThan(60);
    expect(g.bad, "every target at least 44px, inside the screen").toEqual([]);
    // Scrolled to its end, the last choice still reacts.
    await palette(page).evaluate((el) => (el.scrollLeft = el.scrollWidth));
    const last = choices(page).last();
    expect(await inRowView(page, last), "the end of the row is reachable").toBe(true);
    await press(last);
    await expect(sheet(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
  });
});

test.describe("At 48rem and wider", () => {
  test.use({ viewport: { width: 768, height: 760 } });

  test("the footer and the anchored picker are back, and a row tap opens nothing", async ({ page }) => {
    const { own } = await openRoomWithRows(page);
    expect(await narrowLayout(page), "premise: 48rem").toBe(false);
    const display = await own.evaluate((el) => ({
      cluster: getComputedStyle(el.querySelector('[data-testid="message-action-cluster"]')!).display,
      entry: getComputedStyle(el.querySelector('[data-testid="mobile-message-actions-entry"]')!).display,
    }));
    expect(display).toEqual({ cluster: "flex", entry: "none" });
    expect(await tabbableIds(page)).not.toContain("mobile-message-actions-entry");
    await tapRow(page, own);
    await expectNoSheet(page);
    if (!(await coarsePointer(page))) await own.hover({ position: { x: 4, y: 4 } });
    const react = own.getByTestId("message-react-button");
    await expect(react).toHaveAttribute("aria-controls", "reaction-picker");
    await press(react);
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    await expect(sheet(page)).toBeHidden();
  });
});

test.describe("On either side of 48rem", () => {
  // The rules and the press routing use `not all and (min-width: 48rem)`,
  // the exact complement of Tailwind's `md`, so no width is left to neither
  // layout. No engine here lays a page out at a fractional width (a 767.5px
  // frame is snapped to 768), so the two whole widths around it are checked.
  test("the trailing cluster is hidden at 767px and shown at 768px", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    for (const [width, display] of [[767, "none"], [768, "flex"]] as const) {
      await page.setViewportSize({ width, height: 760 });
      const cluster = page.getByTestId("message-action-cluster").first();
      expect(await cluster.evaluate((el) => getComputedStyle(el).display), `${width}px`).toBe(display);
    }
  });
});

test.describe("Inside the gateway's sandboxed frame", () => {
  test("a row tap opens the sheet, and so does +, its first emoji focused", async ({ page }) => {
    // The frame is the variable here, not the device: the desktop engines
    // cover it (a frame inside an emulated phone viewport did not take the
    // room list's clicks).
    test.skip(test.info().project.name.startsWith("mobile"), "covered by the desktop engines");
    const app = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:8082";
    await page.setViewportSize({ width: 600, height: 760 });
    // The gateway's sandbox, plus same-origin so the local build can load.
    await page.setContent(`<!DOCTYPE html><html><body style="margin:0;height:100vh">
      <iframe sandbox="allow-scripts allow-forms allow-popups allow-same-origin" src="${app}"
        style="width:100%;height:100%;border:0;display:block"></iframe></body></html>`);
    const frame = page.frameLocator("iframe");
    await frame.locator(".app-root").waitFor({ timeout: 30_000 });
    await frame.getByTestId("hamburger-rooms-button").filter({ visible: true }).first().click();
    await frame.getByTestId("room-list").getByRole("button", { name: "Your Private Room" }).click();
    const row = frame.getByTestId("message-row").last();
    await expect(row).toBeVisible();
    expect(await row.evaluate((el) => getComputedStyle(el.querySelector('[data-testid="message-action-cluster"]')!).display)).toBe("none");
    const content = row.getByTestId("message-content");
    const cb = (await content.boundingBox())!;
    await content.click({ position: { x: cb.width - 3, y: Math.min(8, cb.height / 2) } });
    const s = frame.getByTestId(SHEET);
    await expect(s).toBeVisible();
    await expect(s).toHaveAttribute("data-target-row", await rowKey(row));
    const panel = await s.locator(".mobile-sheet-panel").boundingBox();
    expect(panel!.y + panel!.height, "bottom-aligned in the frame").toBeGreaterThan(740);
    await frame.getByTestId(BACKDROP).click({ position: { x: 40, y: 120 } });
    await expect(s).toBeHidden();
    await row.getByTestId("message-react-button").click();
    await expect(frame.getByTestId(SHEET)).toBeVisible();
    await expect(frame.getByTestId("mobile-reaction-palette").getByRole("button").first()).toBeFocused();
  });
});
