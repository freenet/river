import { test, expect, Locator, Page } from "@playwright/test";
import {
  chips,
  historyPoint,
  holdChip,
  keyboardFocus,
  openRoomWithComposer,
  openRoomWithTarget,
  openSheetFromReact,
  palette,
  press,
  pressAt,
  rowKey,
  settle,
  SHEET,
  sheet,
  submitMessage,
} from "./example-room";
import { callRiverTest } from "./river-test";

// Below 48rem a message's reaction + and a tap on the row open one message
// sheet (mobile_message_sheet.rs); these are its reaction half
// (mobile-message-actions.spec.ts has the row taps and the actions). A
// reaction chip opens nothing: a press on it reacts with its emoji.
// On top, the 16 choices the desktop picker has, in one row that scrolls
// sideways: a tap on one reacts and closes it. Then who reacted. It has no
// field, so it never asks for the software keyboard. It has no title or
// Close: a tap outside it dismisses it.

const FREQUENT = ["👍", "❤️", "😂", "👀", "🔥", "😮", "😢", "😡", "🎉", "🤔", "👋", "✅", "💯", "🙏", "😍", "👏"];
/// The first choice, for a click queued inside one `page.evaluate`.
const FIRST_CHOICE = `#${SHEET} .mobile-reaction-choice`;

function choices(page: Page): Locator {
  return palette(page).getByRole("button");
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
    await expect(react).toHaveAttribute("aria-controls", SHEET);
    await expect(react).toHaveAttribute("aria-expanded", "false");
    await openSheetFromReact(page, row);
    await expect(react).toHaveAttribute("aria-expanded", "true");
    await expect(sheet(page), "named, though it shows no title").toHaveAccessibleName("Message options");
    await expect(sheet(page).getByRole("heading"), "no title").toHaveCount(0);
    await expect(page.getByTestId("mobile-reaction-close"), "no Close: a tap outside dismisses it").toHaveCount(0);
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
    await expect(sheet(page)).toBeHidden();
    await expect(react, "focus back on the +").toBeFocused();
    expect(await composer.evaluate((el: HTMLTextAreaElement) => [el.value, el.selectionStart, el.selectionEnd])).toEqual(["my unsent draft", 3, 9]);
    await expect(chips(row), "dismissing reacts with nothing").toHaveCount(0);
  });

  test("a tap reacts and closes it; the same emoji removes, another switches", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    await press(page, choices(page).first());
    await expect(sheet(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("👍");
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();

    // The current one is marked, and a tap on it removes it.
    await openSheetFromReact(page, row);
    await press(page, palette(page).getByRole("button", { name: "Remove 👍 reaction" }));
    await expect(chips(row)).toHaveCount(0);

    // Another switches: one chip, the new emoji.
    await openSheetFromReact(page, row);
    await press(page, palette(page).getByRole("button", { name: "React with 🎉" }));
    await expect(chips(row)).toHaveCount(1);
    await openSheetFromReact(page, row);
    await press(page, palette(page).getByRole("button", { name: "React with 😂" }));
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("😂");
  });

  test("the current reaction is centred in the row and focused when it reopens", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    const last = palette(page).getByRole("button", { name: "React with 👏" });
    expect(await inRowView(page, last), "premise: the last choice starts out of view").toBe(false);
    await press(page, last);
    await expect(chips(row)).toContainText("👏");
    await openSheetFromReact(page, row);
    const current = palette(page).getByRole("button", { name: "Remove 👏 reaction" });
    await expect(current).toHaveClass(/mobile-reaction-choice--current/);
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
    await expect(sheet(page), "the new opening is open").toBeVisible();
  });

  test("a chip tap reacts with its emoji and opens nothing; the + opens the sheet, which lists who reacted", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    const party = chips(row).filter({ hasText: "🎉" });
    await expect(party).toContainText("1");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));

    // Who reacted is in the sheet the + opens: the member, not the reader.
    await openSheetFromReact(page, row);
    await expect(sheet(page)).toHaveAttribute("data-target-row", await rowKey(row));
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
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();

    // No reaction of the reader's yet: the tap adds theirs to the chip.
    await press(page, party);
    await expect(party).toContainText("2");
    await expect(sheet(page), "a chip never opens the sheet").toBeHidden();
    await expect(page.getByTestId("reaction-owners"), "nor the reactor list").toHaveCount(0);

    // The same chip again takes it back.
    await press(page, party);
    await expect(party).toContainText("1");

    // Below 48rem a held finger is a tap once it lifts.
    const lift = await holdChip(page, party);
    await expect(page.getByTestId("reaction-owners")).toHaveCount(0);
    await lift();
    await expect(party).toContainText("2");
    await expect(sheet(page)).toBeHidden();
    await press(page, party);
    await expect(party).toContainText("1");

    // And no reactor list for Shift+Enter to open: it reacts, as Enter does.
    await party.press("Shift+Enter");
    await expect(party).toContainText("2");
    await expect(page.getByTestId("reaction-owners")).toHaveCount(0);
    await party.press("Enter");
    await expect(party).toContainText("1");

    // Reacted with another emoji: the tap moves the reaction to the chip's.
    await openSheetFromReact(page, row);
    await press(page, palette(page).getByRole("button", { name: "React with 👍" }));
    await expect(chips(row)).toHaveCount(2);
    await press(page, party);
    await expect(chips(row), "the 👍 left with the switch").toHaveCount(1);
    await expect(party).toContainText("2");
    await expect(sheet(page)).toBeHidden();

    // The sheet then names the reader too, as "You".
    await openSheetFromReact(page, row);
    await expect(sheet(page).getByTestId("mobile-sheet-reactor")).toHaveCount(2);
    await expect(sheet(page).getByTestId("mobile-sheet-reactor").filter({ hasText: /^You$/ })).toHaveCount(1);
    await expect(palette(page).getByRole("button", { name: "Remove 🎉 reaction" }), "and it is their current one").toBeVisible();
  });

  test("opened by a tap its focused emoji shows no ring until a key is pressed; opened by a key it shows at once", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    const outline = () => page.evaluate(() => getComputedStyle(document.activeElement!).outlineStyle);
    await openSheetFromReact(page, row);
    await expect(choices(page).first()).toBeFocused();
    await expect(sheet(page)).toHaveAttribute("data-opened-by", "pointer");
    expect(await outline(), "no ring that reads as the current reaction").toBe("none");
    // Any key in it: the reader is on the keyboard now.
    await page.keyboard.press("Shift");
    await expect(sheet(page)).toHaveAttribute("data-opened-by", "keyboard");
    await page.keyboard.press("Escape");
    await expect(sheet(page)).toBeHidden();

    // From the keyboard: the ring shows straight away.
    await keyboardFocus(page, row.getByTestId("message-react-button"));
    await page.keyboard.press("Enter");
    await expect(sheet(page)).toBeVisible();
    await expect(choices(page).first()).toBeFocused();
    await expect(sheet(page)).toHaveAttribute("data-opened-by", "keyboard");
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
    await press(page, choices(page).first());
    await expect(chips(row)).toHaveCount(1);
  });

  test("a sideways wheel over the row scrolls it", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openSheetFromReact(page, row);
    const history = page.locator("#chat-scroll-container");
    const before = await history.evaluate((el) => el.scrollTop);
    await palette(page).hover();
    await page.mouse.wheel(200, 0);
    await expect.poll(async () => (await rowScroll(page)).left, { message: "the row scrolled" }).toBeGreaterThan(0);
    expect(await history.evaluate((el) => el.scrollTop), "the history did not move").toBe(before);
    await expect(sheet(page)).toBeVisible();
  });

  test("a reaction from the popup that cannot be signed is rolled back, and says so", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "holdMessageSigning");
    await openSheetFromReact(page, row);
    await press(page, choices(page).first());
    await expect(chips(row), "shown while its signature is held").toHaveCount(1);
    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(chips(row), "rolled back").toHaveCount(0);
    await expect(page.getByTestId("toast-message")).toHaveText("Your reaction could not be sent");
    await expect(sheet(page), "nothing reopens").toBeHidden();
  });
});

test.describe("The reaction popup on a small screen", () => {
  test.use({ viewport: { width: 320, height: 360 } });

  test("fits at 320px on the bottom edge, every target at least 44px, in one row whose every choice is reachable", async ({ page }) => {
    // An own message: the sheet at its tallest, with Reply, Edit and Delete.
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "own small-screen row");
    await expect(row).toHaveCount(1);
    await page.getByTestId("message-input").blur();
    await openSheetFromReact(page, row);
    const g = await page.evaluate(() => {
      const panel = document.querySelector("#mobile-message-sheet .mobile-sheet-panel") as HTMLElement;
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
    await press(page, last);
    await expect(sheet(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
  });
});
