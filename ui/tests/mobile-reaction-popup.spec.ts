import { test, expect, Locator, Page } from "@playwright/test";
import {
  committedRow,
  expectTouchHeldUntilClosed,
  historyPoint,
  keyboardFocus,
  openReactionPopup,
  openRoomWithComposer,
  press,
  pressAt,
  rowKey,
  settle,
  withFingers,
} from "./example-room";
import { deliver } from "./history-geometry";
import { callRiverTest, rekeyThenClick } from "./river-test";

// Below 48rem a message's reaction +, a reaction chip and a tap on the row
// open one message sheet (mobile_message_sheet.rs); these are its reaction
// half (mobile-message-actions.spec.ts has the row taps and the actions).
// On top, the 16 choices the desktop picker has, in one row that scrolls
// sideways: a tap on one reacts and closes it. Then who reacted. It has no
// field, so it never asks for the software keyboard. It has no title or
// Close: a tap outside it dismisses it.

const POPUP = "mobile-message-sheet";
const FREQUENT = ["👍", "❤️", "😂", "👀", "🔥", "😮", "😢", "😡", "🎉", "🤔", "👋", "✅", "💯", "🙏", "😍", "👏"];
/// The first choice, for a click queued inside one `page.evaluate`.
const FIRST_CHOICE = "#mobile-message-sheet .mobile-reaction-choice";

function popup(page: Page): Locator {
  return page.getByTestId(POPUP);
}

function palette(page: Page): Locator {
  return page.getByTestId("mobile-reaction-palette");
}

function choices(page: Page): Locator {
  return palette(page).getByRole("button");
}

function chips(row: Locator): Locator {
  return row.getByTestId("reaction-chip");
}

/// A received message with no reactions, at the end of the history.
async function openRoomWithTarget(page: Page, text = "react target"): Promise<Locator> {
  await openRoomWithComposer(page);
  await deliver(page, text);
  const row = committedRow(page, text);
  await expect(row).toHaveCount(1);
  return row;
}

/// The row's sideways scroll and how far it can go.
function rowScroll(page: Page): Promise<{ left: number; max: number }> {
  return palette(page).evaluate((el) => ({ left: el.scrollLeft, max: el.scrollWidth - el.clientWidth }));
}

/// Whether `choice` lies wholly inside the row's visible box.
async function inRowView(page: Page, choice: Locator): Promise<boolean> {
  const [c, p] = [(await choice.boundingBox())!, (await palette(page).boundingBox())!];
  return c.x >= p.x - 1 && c.x + c.width <= p.x + p.width + 1;
}

test.describe("The mobile reaction popup", () => {
  test.use({ viewport: { width: 390, height: 760 } });

  test("+ opens one row of the 16 choices, the first focused, with no field and the composer untouched", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    // A draft and its selection in the composer.
    const composer = page.getByTestId("message-input");
    await composer.fill("my unsent draft");
    await composer.evaluate((el: HTMLTextAreaElement) => el.setSelectionRange(3, 9));
    const react = row.getByTestId("message-react-button");
    await expect(react).toHaveAttribute("aria-haspopup", "dialog");
    await expect(react).toHaveAttribute("aria-controls", POPUP);
    await expect(react).toHaveAttribute("aria-expanded", "false");
    await openReactionPopup(page, row);
    await expect(react).toHaveAttribute("aria-expanded", "true");
    await expect(popup(page), "named, though it shows no title").toHaveAccessibleName("Message options");
    await expect(popup(page).getByRole("heading"), "no title").toHaveCount(0);
    await expect(page.getByTestId("mobile-reaction-close"), "no Close: a tap outside dismisses it").toHaveCount(0);
    await expect(popup(page).getByTestId("mobile-message-action-reply"), "the actions below the emoji").toBeVisible();
    await expect(popup(page).getByTestId("mobile-sheet-reactors"), "no reactions, no list").toHaveCount(0);
    await expect(popup(page).locator("input, textarea, [contenteditable]"), "no field to raise a keyboard").toHaveCount(0);
    await expect(palette(page)).toHaveAttribute("role", "group");
    await expect(palette(page)).toHaveAccessibleName("Reactions");
    expect(await choices(page).allTextContents(), "every one of the existing 16, in order").toEqual(FREQUENT);
    await expect(choices(page).first()).toHaveAccessibleName("React with 👍");
    await expect(choices(page).first(), "the first choice takes focus").toBeFocused();
    const g = await palette(page).evaluate((el) => ({
      tops: new Set(Array.from(el.children).map((c) => Math.round(c.getBoundingClientRect().top))).size,
      overflowX: getComputedStyle(el).overflowX,
      scrolls: el.scrollWidth > el.clientWidth,
      left: el.scrollLeft,
    }));
    expect(g.tops, "one row").toBe(1);
    expect(g.overflowX).toBe("auto");
    expect(g.scrolls, "wider than the panel: it scrolls sideways").toBe(true);
    expect(g.left, "it opens at the start").toBe(0);
    // A tap outside, over the history, lands on the backdrop and dismisses it.
    const outside = await historyPoint(page);
    expect(await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid"), outside)).toBe("mobile-message-sheet-backdrop");
    await pressAt(page, outside);
    await expect(popup(page)).toBeHidden();
    await expect(react, "focus back on the +").toBeFocused();
    expect(await composer.evaluate((el: HTMLTextAreaElement) => [el.value, el.selectionStart, el.selectionEnd])).toEqual(["my unsent draft", 3, 9]);
    await expect(chips(row), "dismissing reacts with nothing").toHaveCount(0);
  });

  test("a tap reacts and closes it; the same emoji removes, another switches", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await press(page, choices(page).first());
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("👍");
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();

    // The current one is marked, and a tap on it removes it.
    await openReactionPopup(page, row);
    await press(page, palette(page).getByRole("button", { name: "Remove 👍 reaction" }));
    await expect(chips(row)).toHaveCount(0);

    // Another switches: one chip, the new emoji.
    await openReactionPopup(page, row);
    await press(page, palette(page).getByRole("button", { name: "React with 🎉" }));
    await expect(chips(row)).toHaveCount(1);
    await openReactionPopup(page, row);
    await press(page, palette(page).getByRole("button", { name: "React with 😂" }));
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("😂");
  });

  test("the current reaction is centred in the row and focused when it reopens", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    const last = palette(page).getByRole("button", { name: "React with 👏" });
    expect(await inRowView(page, last), "premise: the last choice starts out of view").toBe(false);
    await press(page, last);
    await expect(chips(row)).toContainText("👏");
    await openReactionPopup(page, row);
    const current = palette(page).getByRole("button", { name: "Remove 👏 reaction" });
    await expect(current).toHaveClass(/mobile-reaction-choice--current/);
    await expect(current, "the current reaction takes focus").toBeFocused();
    expect(await inRowView(page, current), "scrolled into the row's view").toBe(true);
    const { left, max } = await rowScroll(page);
    expect(left, "the last one: the row at its end").toBeGreaterThan(max - 2);
  });

  test("a duplicate tap acts once and does not undo itself", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
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
    await openReactionPopup(page, row);
    const key = await rowKey(row);
    // In one task: a tap outside, + again on the same message, then the old
    // choice.
    await page.evaluate(
      ({ key, sel }) => {
        const choice = document.querySelector<HTMLElement>(sel)!;
        const close = document.querySelector<HTMLElement>('[data-testid="mobile-message-sheet-backdrop"]')!;
        const plus = document.querySelector<HTMLElement>(`[id="msg-${key}"] [data-testid="message-react-button"]`)!;
        close.click();
        plus.click();
        choice.click();
      },
      { key, sel: FIRST_CHOICE },
    );
    await settle(page);
    await expect(chips(row), "the earlier opening's tap did nothing").toHaveCount(0);
    await expect(popup(page), "the new opening is open").toBeVisible();
  });

  test("Escape and the backdrop close it without reacting, and the backdrop tap reaches nothing", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await page.keyboard.press("Escape");
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);
    await openReactionPopup(page, row);
    // Over the header's rooms button.
    const hamburger = (await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).first().boundingBox())!;
    const point = { x: hamburger.x + hamburger.width / 2, y: hamburger.y + hamburger.height / 2 };
    expect(await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid"), point)).toBe("mobile-message-sheet-backdrop");
    await pressAt(page, point);
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);
    await expect(page.locator("#chat-scroll-container"), "the rooms panel did not open").toBeVisible();
  });

  test("a chip opens the sheet, which lists who reacted; dismissed, focus returns to the chip", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    const chip = chips(row);
    await expect(chip).toHaveCount(1);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await press(page, chip);
    await expect(popup(page)).toBeVisible();
    await expect(popup(page)).toHaveAttribute("data-target-row", await rowKey(row));
    await expect(page.getByTestId("reaction-owners"), "not the desktop reactor list").toHaveCount(0);
    const list = popup(page).getByTestId("mobile-sheet-reactors");
    await expect(list).toHaveAccessibleName("Who reacted");
    await expect(list.getByTestId("mobile-sheet-reaction")).toHaveCount(1);
    await expect(list.getByTestId("mobile-sheet-reaction")).toContainText("🎉");
    const names = await list.getByTestId("mobile-sheet-reactor").allTextContents();
    expect(names, "the member who reacted, not the reader").toHaveLength(1);
    expect(names[0]).not.toBe("You");
    const [paletteBox, listBox, actionsBox] = [
      await palette(page).boundingBox(),
      await list.boundingBox(),
      await popup(page).locator(".mobile-sheet-actions").boundingBox(),
    ];
    expect(paletteBox!.y, "emoji, then who reacted").toBeLessThan(listBox!.y);
    expect(listBox!.y, "then the actions").toBeLessThan(actionsBox!.y);
    await expect(choices(page).first(), "an emoji takes focus").toBeFocused();
    await page.keyboard.press("Escape");
    await expect(popup(page)).toBeHidden();
    await expect(chip, "focus back on the chip it opened from").toBeFocused();

    // The reader joins in: the list then names them too, as "You".
    await openReactionPopup(page, row);
    await press(page, palette(page).getByRole("button", { name: "React with 🎉" }));
    await expect(chip).toContainText("2");
    await press(page, chip);
    await expect(popup(page).getByTestId("mobile-sheet-reactor")).toHaveCount(2);
    await expect(popup(page).getByTestId("mobile-sheet-reactor").filter({ hasText: /^You$/ })).toHaveCount(1);
    await expect(palette(page).getByRole("button", { name: "Remove 🎉 reaction" }), "and it is their current one").toBeVisible();
  });

  test("Enter on a chip opens the sheet too", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    const chip = chips(row);
    await expect(chip).toHaveCount(1);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await keyboardFocus(page, chip);
    await page.keyboard.press("Enter");
    await expect(popup(page)).toBeVisible();
    await expect(page.getByTestId("reaction-owners")).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(chip).toBeFocused();
  });

  test("its message leaving closes it; a remount keeps it", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "deliverTimedMessages", 0, [
      { text: "popup lead", sentSecs: 1, receivedSecs: 0 },
      { text: "popup survivor", sentSecs: 2, receivedSecs: 0 },
    ]);
    const survivor = committedRow(page, "popup survivor");
    await expect(survivor).toHaveCount(1);
    await openReactionPopup(page, survivor);
    await survivor.evaluate((el) => ((window as unknown as { __old: Element }).__old = el));
    await callRiverTest(page, "removeMessages", "popup lead");
    await expect(committedRow(page, "popup lead")).toHaveCount(0);
    await settle(page);
    expect(await survivor.evaluate((el) => el === (window as unknown as { __old: Element }).__old), "premise: remounted").toBe(false);
    await expect(popup(page), "a remount keeps it").toBeVisible();
    await press(page, choices(page).first());
    await expect(chips(survivor), "and a tap acts on it, once").toHaveCount(1);
    await expect(survivor.getByTestId("message-react-button"), "focus to the new +").toBeFocused();

    await openReactionPopup(page, survivor);
    await callRiverTest(page, "removeMessages", "popup survivor");
    await expect(popup(page), "its message left").toBeHidden();
  });

  test("open, a reload waits; closed, it does not", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await page.getByTestId("message-input").blur();
    expect(await callRiverTest(page, "userIsBusy"), "premise: idle").toBe(false);
    await openReactionPopup(page, row);
    expect(await callRiverTest(page, "userIsBusy"), "open is busy").toBe(true);
    await page.keyboard.press("Escape");
    await expect(popup(page)).toBeHidden();
    await row.getByTestId("message-react-button").blur();
    expect(await callRiverTest(page, "userIsBusy"), "closed and mounted: not busy").toBe(false);
  });

  test("without the Popover API the + still opens it", { tag: "@chromium-only" }, async ({ page }) => {
    await page.addInitScript(() => {
      Object.defineProperty(HTMLElement.prototype, "showPopover", { value: undefined, configurable: true, writable: true });
    });
    const row = await openRoomWithTarget(page);
    const react = row.getByTestId("message-react-button");
    await expect(react).not.toHaveAttribute("aria-disabled");
    await openReactionPopup(page, row);
    await press(page, choices(page).first());
    await expect(chips(row)).toHaveCount(1);
  });

  // The hold and the close paths both surfaces share are the actions
  // sheet's test; these are the popup's own.
  test("the touch hold lets go when a choice or its message leaving closes it", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await expectTouchHeldUntilClosed(page, POPUP, () => openReactionPopup(page, row), [
      ["a choice", () => press(page, choices(page).first())],
      // Last: it takes the row away.
      ["its message leaving", () => callRiverTest(page, "removeMessages", "react target")],
    ]);
  });

  test("a sideways wheel over the row scrolls it", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    const history = page.locator("#chat-scroll-container");
    const before = await history.evaluate((el) => el.scrollTop);
    await palette(page).hover();
    await page.mouse.wheel(200, 0);
    await expect.poll(async () => (await rowScroll(page)).left, { message: "the row scrolled" }).toBeGreaterThan(0);
    expect(await history.evaluate((el) => el.scrollTop), "the history did not move").toBe(before);
    await expect(popup(page)).toBeVisible();
  });

  test("a re-key closes it, and a tap queued across it does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await rekeyThenClick(page, FIRST_CHOICE);
    await settle(page);
    await expect(popup(page)).toBeHidden();
    await expect(chips(row), "no reaction from the earlier generation's opening").toHaveCount(0);
    expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
    await openReactionPopup(page, row);
  });

  test("a reaction from the popup that cannot be signed is rolled back, and says so", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "holdMessageSigning");
    await openReactionPopup(page, row);
    await press(page, choices(page).first());
    await expect(chips(row), "shown while its signature is held").toHaveCount(1);
    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(chips(row), "rolled back").toHaveCount(0);
    await expect(page.getByTestId("toast-message")).toHaveText("Your reaction could not be sent");
    await expect(popup(page), "nothing reopens").toBeHidden();
  });

  test("opening and closing it renders no message group", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await settle(page);
    await callRiverTest(page, "takeMessageGroupRenders");
    await openReactionPopup(page, row);
    await page.keyboard.press("Escape");
    await expect(popup(page)).toBeHidden();
    await settle(page);
    expect(await callRiverTest(page, "takeMessageGroupRenders")).toBe(0);
  });
});

test.describe("A finger on the reaction popup's row", () => {
  test.use({ viewport: { width: 390, height: 760 } });
  test.skip(({ browserName }) => browserName !== "chromium", "real touch input needs Chromium's input protocol");

  test("a sideways swipe scrolls the row, not the history, and is not a tap", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    const history = page.locator("#chat-scroll-container");
    const before = await history.evaluate((el) => el.scrollTop);
    const box = (await palette(page).boundingBox())!;
    const [x, y] = [Math.round(box.x + box.width - 30), Math.round(box.y + box.height / 2)];
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [-200, 0] });
      await fingers.lift();
    });
    await settle(page);
    expect((await rowScroll(page)).left, "the row scrolled").toBeGreaterThan(50);
    expect(await history.evaluate((el) => el.scrollTop), "the history did not move").toBe(before);
    await expect(popup(page), "a swipe is not a tap: still open").toBeVisible();
    await expect(chips(row)).toHaveCount(0);
  });
});

test.describe("The reaction popup on a short screen", () => {
  test.use({ viewport: { width: 320, height: 360 } });

  test("fits at 320px in one row that scrolls sideways, and every choice is reachable", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    const g = await page.evaluate(() => {
      const panel = document.querySelector("#mobile-message-sheet .mobile-sheet-panel") as HTMLElement;
      const r = panel.getBoundingClientRect();
      const palette = panel.querySelector(".mobile-reaction-palette") as HTMLElement;
      return {
        vw: document.documentElement.clientWidth,
        vh: window.innerHeight,
        top: r.top,
        bottom: r.bottom,
        left: r.left,
        right: r.right,
        rowHeight: palette.getBoundingClientRect().height,
        overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth,
        small: Array.from(panel.querySelectorAll("button")).filter((b) => {
          const br = b.getBoundingClientRect();
          return br.height < 44 || br.width < 44;
        }).length,
      };
    });
    expect(g.overflowX, "nothing overflows the page").toBe(false);
    expect(g.top, "inside the viewport").toBeGreaterThanOrEqual(0);
    expect(g.bottom).toBeLessThanOrEqual(g.vh + 1);
    expect(g.left).toBeGreaterThanOrEqual(0);
    expect(g.right).toBeLessThanOrEqual(g.vw);
    expect(g.rowHeight, "one row").toBeLessThan(60);
    expect(g.small, "every target at least 44px").toBe(0);
    // Scrolled to its end, the last choice still reacts.
    await palette(page).evaluate((el) => (el.scrollLeft = el.scrollWidth));
    const last = choices(page).last();
    expect(await inRowView(page, last), "the end of the row is reachable").toBe(true);
    await press(page, last);
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
  });
});
