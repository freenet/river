import { test, expect, Locator, Page } from "@playwright/test";
import {
  committedRow,
  expectTouchHeldUntilClosed,
  keyboardFocus,
  openReactionPopup,
  openRoomWithComposer,
  press,
  pressAt,
  rowKey,
  settle,
} from "./example-room";
import { deliver } from "./history-geometry";
import { callRiverTest, rekeyThenClick } from "./river-test";

// Below 48rem a message's reaction + opens a separate reaction popup
// (mobile_reaction_input.rs): a labelled field for the device keyboard's
// emoji, an explicit React/Remove submit, and "Choose from palette" with the
// 16 choices the desktop picker has. It never opens the row's actions. What
// these tests cannot show is the software keyboard itself: emulated touch
// proves the app's event handling and focus, not that a real iOS Safari or
// Android Chrome raised its keyboard (see the plan's device record).

const POPUP = "mobile-reaction-popup";

function popup(page: Page): Locator {
  return page.getByTestId(POPUP);
}

function input(page: Page): Locator {
  return page.getByTestId("mobile-reaction-input");
}

function chips(row: Locator): Locator {
  return row.getByTestId("reaction-chip");
}

/// Type `text` into the field the way a keyboard's emoji key delivers it.
async function typeEmoji(page: Page, text: string) {
  await input(page).focus();
  await page.keyboard.insertText(text);
  await expect(input(page)).toHaveValue(text);
}

/// The code points of each chip's emoji on `row`.
function chipCodepoints(row: Locator): Promise<string[]> {
  return chips(row).evaluateAll((els) =>
    els.map((el) => Array.from(el.querySelector("span")!.textContent!).map((c) => c.codePointAt(0)!.toString(16)).join(" ")),
  );
}

/// A received message with no reactions, at the end of the history.
async function openRoomWithTarget(page: Page, text = "react target"): Promise<Locator> {
  await openRoomWithComposer(page);
  await deliver(page, text);
  const row = committedRow(page, text);
  await expect(row).toHaveCount(1);
  return row;
}

test.describe("The mobile reaction popup", () => {
  test.use({ viewport: { width: 390, height: 760 } });

  test("+ opens it, labelled, with its field focused and the composer untouched", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    // A draft, its selection and a reply in the composer.
    const composer = page.getByTestId("message-input");
    await composer.fill("my unsent draft");
    await composer.evaluate((el: HTMLTextAreaElement) => el.setSelectionRange(3, 9));
    const react = row.getByTestId("message-react-button");
    await expect(react).toHaveAttribute("aria-haspopup", "dialog");
    await expect(react).toHaveAttribute("aria-controls", POPUP);
    await expect(react).toHaveAttribute("aria-expanded", "false");
    await openReactionPopup(page, row);
    await expect(react).toHaveAttribute("aria-expanded", "true");
    await expect(popup(page).getByRole("heading", { name: "React to message" })).toBeVisible();
    await expect(input(page), "the field takes focus on the press").toBeFocused();
    await expect(input(page)).toHaveAccessibleName("Emoji");
    await expect(input(page)).toHaveValue("");
    await expect(input(page), "no maxlength cuts a sequence").not.toHaveAttribute("maxlength");
    await expect(popup(page).getByText("Use your keyboard’s emoji key, or choose from the palette.")).toBeVisible();
    await expect(page.getByTestId("mobile-message-actions-sheet"), "never the actions").toBeHidden();
    await expect(popup(page).getByText(/Reply|Edit|Delete/)).toHaveCount(0);
    await expect(page.getByTestId("mobile-reaction-palette"), "the palette waits to be asked for").toHaveCount(0);
    await typeEmoji(page, "🔥");
    await press(page, page.getByTestId("mobile-reaction-close"));
    await expect(popup(page)).toBeHidden();
    await expect(react, "focus back on the +").toBeFocused();
    expect(await composer.evaluate((el: HTMLTextAreaElement) => [el.value, el.selectionStart, el.selectionEnd])).toEqual(["my unsent draft", 3, 9]);
    await expect(chips(row), "Close reacts with nothing").toHaveCount(0);
  });

  test("typing never submits; React reacts once with the exact bytes typed", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    // A text-default heart with its VS16, a skin-toned ZWJ sequence: kept as typed.
    await typeEmoji(page, "❤️");
    await settle(page);
    await expect(popup(page), "typing does not submit").toBeVisible();
    await expect(chips(row)).toHaveCount(0);
    await expect(page.getByTestId("mobile-reaction-submit")).toHaveText("React");
    await press(page, page.getByTestId("mobile-reaction-submit"));
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
    expect(await chipCodepoints(row), "byte for byte, VS16 kept").toEqual(["2764 fe0f"]);
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();

    // A ZWJ sequence with a skin tone switches to it, whole.
    await openReactionPopup(page, row);
    await expect(input(page), "a new opening starts empty").toHaveValue("");
    await typeEmoji(page, "🧑🏽‍🚀");
    await page.keyboard.press("Enter");
    await expect(popup(page)).toBeHidden();
    expect(await chipCodepoints(row)).toEqual(["1f9d1 1f3fd 200d 1f680"]);
  });

  test("Enter submits only once a composition has finished, in either event order", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await typeEmoji(page, "😂");
    // Chromium's and Firefox's order: the confirming Enter during the
    // composition, then compositionend.
    await page.evaluate(() => {
      const field = document.getElementById("mobile-reaction-input")!;
      field.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
      field.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
      field.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", isComposing: true, bubbles: true, cancelable: true }));
    });
    await settle(page);
    await expect(popup(page), "Enter during a composition confirms it, never submits").toBeVisible();
    await expect(chips(row)).toHaveCount(0);
    await page.evaluate(() => {
      document.getElementById("mobile-reaction-input")!.dispatchEvent(new CompositionEvent("compositionend", { data: "😂", bubbles: true }));
    });
    await settle(page);
    await expect(chips(row), "the composition ending submits nothing").toHaveCount(0);
    // WebKit's order: compositionend first, then the confirming Enter, which
    // says isComposing: false and keyCode 229.
    await page.evaluate(() => {
      const field = document.getElementById("mobile-reaction-input")!;
      const ime = () => {
        const e = new KeyboardEvent("keydown", { key: "Enter", isComposing: false, bubbles: true, cancelable: true });
        Object.defineProperty(e, "keyCode", { get: () => 229 });
        return e;
      };
      field.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true }));
      field.dispatchEvent(new CompositionEvent("compositionend", { data: "😂", bubbles: true }));
      field.dispatchEvent(ime());
      // A 229 keydown a task later is still the input method's.
      setTimeout(() => field.dispatchEvent(ime()), 20);
    });
    await settle(page);
    await expect(popup(page), "the conversion's Enter did not submit").toBeVisible();
    await expect(chips(row)).toHaveCount(0);
    await page.keyboard.press("Enter");
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
  });

  test("text, several emoji and nothing are explained; the field stays editable", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    const error = page.getByTestId("mobile-reaction-error");
    for (const [typed, says] of [
      ["", "Type or paste one emoji."],
      ["ok", "That isn’t an emoji River knows. Choose one from the palette."],
      ["👍👍", "One emoji per reaction."],
    ]) {
      await input(page).fill(typed);
      await press(page, page.getByTestId("mobile-reaction-submit"));
      await expect(error).toHaveText(says);
      await expect(error).toHaveAttribute("role", "alert");
      await expect(input(page)).toHaveAttribute("aria-invalid", "true");
      await expect(popup(page)).toBeVisible();
      await expect(input(page), "what was typed stays").toHaveValue(typed);
    }
    await expect(chips(row)).toHaveCount(0);
    // Editing clears the explanation; a valid emoji then reacts.
    await input(page).fill("👍");
    await expect(error).toHaveCount(0);
    await press(page, page.getByTestId("mobile-reaction-submit"));
    await expect(chips(row)).toHaveCount(1);
  });

  test("the same emoji removes, another switches, and the palette commits in one tap", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
    const palette = page.getByTestId("mobile-reaction-palette");
    await expect(palette).toBeVisible();
    await expect(page.getByTestId("mobile-reaction-palette-toggle")).toHaveAttribute("aria-expanded", "true");
    const choices = palette.getByRole("button");
    await expect(choices, "every one of the existing 16").toHaveCount(16);
    expect(await choices.allTextContents()).toEqual(["👍", "❤️", "😂", "👀", "🔥", "😮", "😢", "😡", "🎉", "🤔", "👋", "✅", "💯", "🙏", "😍", "👏"]);
    await press(page, choices.first());
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("👍");

    // Typing the reaction you already have says it removes it, and does.
    await openReactionPopup(page, row);
    await typeEmoji(page, "👍");
    await expect(page.getByTestId("mobile-reaction-submit")).toHaveText("Remove");
    await input(page).fill("😂");
    await expect(page.getByTestId("mobile-reaction-submit")).toHaveText("React");
    await input(page).fill("👍");
    await press(page, page.getByTestId("mobile-reaction-submit"));
    await expect(chips(row)).toHaveCount(0);

    // A palette choice switches: one chip, the new emoji.
    await openReactionPopup(page, row);
    await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
    await press(page, page.getByTestId("mobile-reaction-palette").getByRole("button", { name: "React with 🎉" }));
    await expect(chips(row)).toHaveCount(1);
    await openReactionPopup(page, row);
    await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
    await expect(page.getByTestId("mobile-reaction-palette").getByRole("button", { name: "Remove 🎉 reaction" }), "the current one is marked").toBeVisible();
    await press(page, page.getByTestId("mobile-reaction-palette").getByRole("button", { name: "React with 😂" }));
    await expect(chips(row)).toHaveCount(1);
    await expect(chips(row)).toContainText("😂");
  });

  test("a duplicate submit acts once and does not undo itself", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await typeEmoji(page, "🔥");
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await page.evaluate(() => {
      const submit = document.querySelector<HTMLElement>('[data-testid="mobile-reaction-submit"]')!;
      submit.click();
      submit.click();
      document.getElementById("mobile-reaction-input")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    });
    await settle(page);
    await expect(chips(row), "one reaction, not toggled back off").toHaveCount(1);
    await expect(chips(row)).toContainText("🔥");
    await expect.poll(() => callRiverTest(page, "messageSignsRecorded"), { message: "signed once" }).toBe(signs + 1);
  });

  test("a submit queued before a dismissal and reopening of the same message does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await typeEmoji(page, "🔥");
    const key = await rowKey(row);
    // In one task: Close, + again on the same message, then the old submit.
    await page.evaluate((key) => {
      const submit = document.querySelector<HTMLElement>('[data-testid="mobile-reaction-submit"]')!;
      const close = document.querySelector<HTMLElement>('[data-testid="mobile-reaction-close"]')!;
      const plus = document.querySelector<HTMLElement>(`[id="msg-${key}"] [data-testid="message-react-button"]`)!;
      close.click();
      plus.click();
      submit.click();
    }, key);
    await settle(page);
    await expect(chips(row), "the earlier opening's submit did nothing").toHaveCount(0);
    await expect(popup(page), "the new opening is open").toBeVisible();
    await expect(input(page), "and starts empty").toHaveValue("");
  });

  test("Escape and the backdrop discard what was typed, and the backdrop tap reaches nothing", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await typeEmoji(page, "🔥");
    await page.keyboard.press("Escape");
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);
    await openReactionPopup(page, row);
    await expect(input(page)).toHaveValue("");
    await typeEmoji(page, "🔥");
    // Over the header's rooms button.
    const hamburger = (await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).first().boundingBox())!;
    const point = { x: hamburger.x + hamburger.width / 2, y: hamburger.y + hamburger.height / 2 };
    expect(await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid"), point)).toBe("mobile-reaction-backdrop");
    await pressAt(page, point);
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);
    await expect(page.locator("#chat-scroll-container"), "the rooms panel did not open").toBeVisible();
  });

  test("closing a reactor list for the popup leaves focus in the field", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    const chip = chips(row);
    await expect(chip).toHaveCount(1);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    // From the keyboard: the chip's list, then Tab to the + and Enter.
    await keyboardFocus(page, chip);
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("reaction-owners")).toBeVisible();
    await row.getByTestId("message-react-button").focus();
    await page.keyboard.press("Enter");
    await expect(popup(page)).toBeVisible();
    await expect(page.getByTestId("reaction-owners"), "the list closed").toHaveCount(0);
    await settle(page);
    await expect(input(page), "its focus return did not take the field's focus").toBeFocused();
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
    await typeEmoji(page, "🔥");
    await survivor.evaluate((el) => ((window as unknown as { __old: Element }).__old = el));
    await callRiverTest(page, "removeMessages", "popup lead");
    await expect(committedRow(page, "popup lead")).toHaveCount(0);
    await settle(page);
    expect(await survivor.evaluate((el) => el === (window as unknown as { __old: Element }).__old), "premise: remounted").toBe(false);
    await expect(popup(page), "a remount keeps it").toBeVisible();
    await expect(input(page), "with what was typed").toHaveValue("🔥");
    await press(page, page.getByTestId("mobile-reaction-submit"));
    await expect(chips(survivor), "and a submit acts on it, once").toHaveCount(1);
    await expect(survivor.getByTestId("message-react-button"), "focus to the new +").toBeFocused();

    await openReactionPopup(page, survivor);
    await callRiverTest(page, "removeMessages", "popup survivor");
    await expect(popup(page), "its message left").toBeHidden();
  });

  test("open with an empty field, a reload waits; closed, it does not", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await page.getByTestId("message-input").blur();
    expect(await callRiverTest(page, "userIsBusy"), "premise: idle").toBe(false);
    await openReactionPopup(page, row);
    await expect(input(page)).toHaveValue("");
    expect(await callRiverTest(page, "userIsBusy"), "an empty field open is busy").toBe(true);
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
    await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
    await press(page, page.getByTestId("mobile-reaction-palette").getByRole("button").first());
    await expect(chips(row)).toHaveCount(1);
  });

  // The hold and the close paths both surfaces share are the actions
  // sheet's test; these are the popup's own.
  test("the touch hold lets go when a palette pick or its message leaving closes it", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await expectTouchHeldUntilClosed(page, POPUP, () => openReactionPopup(page, row), [
      ["a palette pick", async () => {
        await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
        await press(page, page.getByTestId("mobile-reaction-palette").getByRole("button").first());
      }],
      // Last: it takes the row away.
      ["its message leaving", () => callRiverTest(page, "removeMessages", "react target")],
    ]);
  });

  test("a re-key closes it, and a submit queued across it does nothing", { tag: "@chromium-only" }, async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await typeEmoji(page, "🔥");
    const signs = await callRiverTest(page, "messageSignsRecorded");
    await rekeyThenClick(page, "mobile-reaction-submit");
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
    await typeEmoji(page, "🔥");
    await press(page, page.getByTestId("mobile-reaction-submit"));
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
    await typeEmoji(page, "🔥");
    await page.keyboard.press("Escape");
    await expect(popup(page)).toBeHidden();
    await settle(page);
    expect(await callRiverTest(page, "takeMessageGroupRenders")).toBe(0);
  });
});

test.describe("The reaction popup on a short screen", () => {
  test.use({ viewport: { width: 320, height: 360 } });

  test("fits at 320px, scrolls itself with the palette out, and keeps every control reachable", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await openReactionPopup(page, row);
    await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
    await expect(page.getByTestId("mobile-reaction-palette")).toBeVisible();
    const g = await page.evaluate(() => {
      const panel = document.querySelector("#mobile-reaction-popup .mobile-sheet-panel") as HTMLElement;
      const r = panel.getBoundingClientRect();
      return {
        vw: document.documentElement.clientWidth,
        vh: window.innerHeight,
        top: r.top,
        bottom: r.bottom,
        left: r.left,
        right: r.right,
        scrolls: panel.scrollHeight > panel.clientHeight,
        overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth,
        small: Array.from(panel.querySelectorAll("button, input")).filter((b) => {
          const br = b.getBoundingClientRect();
          return br.height < 44 || br.width < 44;
        }).length,
      };
    });
    expect(g.overflowX).toBe(false);
    expect(g.top, "inside the viewport").toBeGreaterThanOrEqual(0);
    expect(g.bottom).toBeLessThanOrEqual(g.vh + 1);
    expect(g.left).toBeGreaterThanOrEqual(0);
    expect(g.right).toBeLessThanOrEqual(g.vw);
    expect(g.scrolls, "the palette makes it scroll, not overflow").toBe(true);
    expect(g.small, "every target at least 44px").toBe(0);
    // Scrolling inside it reaches the last choice, which still reacts.
    const last = page.getByTestId("mobile-reaction-palette").getByRole("button").last();
    await last.scrollIntoViewIfNeeded();
    await press(page, last);
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(1);
  });
});
