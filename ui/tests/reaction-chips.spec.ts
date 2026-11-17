import { expect, Locator, Page, test } from "@playwright/test";
import {
  chipFor as chip,
  chips,
  committedRow,
  expectClear,
  expectUsableLeftGroup,
  footerLayout,
  hasTouch,
  keyboardFocus,
  openInlinePicker,
  openRoomWithTarget,
  openSheetFromReact,
  palette,
  press,
  reactionChoices,
  settle,
  sheet,
} from "./example-room";
import { callRiverTest } from "./river-test";

// A reaction chip is a native toggle button: a press of any kind reacts with
// its emoji (adds, switches or takes back the reader's one reaction) and opens
// nothing. Who reacted is read as on main: the hover title, and at 48rem and
// wider a held finger or Shift+Enter / Shift+Space for the reactor list
// (reaction-owners-touch.spec.ts). Chips stand in the order of their earliest
// still-standing reaction, and a chip that order moves keeps its focus.

const owners = (page: Page) => page.getByTestId("reaction-owners");
const count = (target: Locator) => target.getByTestId("reaction-chip-count");
const order = (row: Locator) => chips(row).evaluateAll((els) => els.map((el) => el.getAttribute("data-emoji")));
const signs = (page: Page) => callRiverTest(page, "messageSignsRecorded");

/// Panics and unreachable code the page reports, collected from here on.
function panicsOf(page: Page): string[] {
  const panics: string[] = [];
  page.on("pageerror", (e) => panics.push(e.message));
  page.on("console", (m) => {
    if (m.type() === "error" && /panicked|unreachable|already borrowed/.test(m.text())) panics.push(m.text());
  });
  return panics;
}

async function expectChip(target: Locator, pressed: boolean, n: string, what = "") {
  await expect(target, `${what} aria-pressed`).toHaveAttribute("aria-pressed", String(pressed));
  await expect(count(target), `${what} count`).toHaveText(n);
}

/// React to `row` with `emoji` through its footer React.
async function pick(page: Page, row: Locator, emoji: string) {
  await openInlinePicker(page, row);
  await press((await reactionChoices(page)).filter({ hasText: new RegExp(`^${emoji}$`, "u") }));
  await expect(chip(row, emoji)).toHaveAttribute("aria-pressed", "true");
}

/// How far the history's view is from its end. Not `scrollTop`: a signed
/// commit can regroup rows above a view held at the end.
const distanceFromEnd = (page: Page) =>
  page.evaluate(() => {
    const scroller = document.getElementById("chat-scroll-container")!;
    return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
  });

const inHistoryView = (target: Locator) =>
  target.evaluate((el) => {
    const box = el.getBoundingClientRect();
    const view = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return box.bottom > view.top && box.top < view.bottom;
  });

test("a chip is a native toggle button with its emoji, its count and the reactors' names on hover", async ({ page }) => {
  const row = await openRoomWithTarget(page);
  await callRiverTest(page, "memberReactsToNewest", "🦀");
  const crab = chip(row, "🦀");
  await expect(crab).toHaveJSProperty("tagName", "BUTTON");
  await expect(crab).toHaveAttribute("type", "button");
  await expect(crab).toHaveAttribute("id", /^reaction-chip-/);
  await expect(crab).toHaveRole("button");
  await expect(crab, "Tab reaches it as a button").toHaveJSProperty("tabIndex", 0);
  for (const attr of ["role", "tabindex", "aria-label", "aria-haspopup", "aria-controls", "aria-expanded"]) {
    await expect(crab, `no ${attr}`).not.toHaveAttribute(attr);
  }
  await expect(crab.locator("span").first()).toHaveText("🦀");
  await expectChip(crab, false, "1");
  await expect(crab, "another member's names").toHaveAttribute("title", /^Test Sender [AB]$/);

  await crab.click();
  await expectChip(crab, true, "2");
  await expect(crab, "the reader is named").toHaveAttribute("title", /(^|, )You(, | \()/);
  await expect(crab, "and told a click takes it back").toHaveAttribute("title", / \(click to remove\)$/);
  await expect(crab, "still no list").not.toHaveAttribute("aria-expanded");
});

type Press = { name: string; touch?: boolean; run: (target: Locator) => Promise<void> };

const PRESSES: Press[] = [
  { name: "a touch tap", touch: true, run: (target) => target.tap() },
  { name: "a mouse click", run: (target) => target.click() },
  { name: "Enter", run: (target) => target.press("Enter") },
  { name: "Space", run: (target) => target.press("Space") },
  // Assistive technology, or a script: a click with no pointer or key press.
  { name: "a click with no press of its own", run: (target) => target.evaluate((el) => (el as HTMLElement).click()) },
];

for (const width of [390, 1280]) {
  test.describe(`Chip presses at ${width}px`, () => {
    test.beforeEach(async ({ page }) => page.setViewportSize({ width, height: 800 }));

    for (const p of PRESSES) {
      test(`${p.name} toggles and switches the reader's reaction, once, and opens nothing`, async ({ page }) => {
        test.skip(p.touch === true && !hasTouch(), "a finger, on a touch project");
        const row = await openRoomWithTarget(page);
        await callRiverTest(page, "memberReactsToNewest", "🦀");
        await callRiverTest(page, "memberReactsToNewest", "❤️");
        const crab = chip(row, "🦀");
        const heart = chip(row, "❤️");
        await expectChip(crab, false, "1", "premise: 🦀");
        await expectChip(heart, false, "1", "premise: ❤️");

        // Each step is checked again once everything has landed: a press that
        // reacted twice would end where it started. Each action is one signed
        // message, so the signatures also catch a press that reacted twice
        // before the first had reached the outbox: both add the same emoji,
        // and adding twice ends as adding once.
        const step = async (target: Locator, expected: [Locator, boolean, string][], actions: number, what: string) => {
          const before = await signs(page);
          await p.run(target);
          for (const [c, pressed, n] of expected) await expectChip(c, pressed, n, what);
          await settle(page);
          for (const [c, pressed, n] of expected) await expectChip(c, pressed, n, `${what}, once`);
          expect(await signs(page), `${what}: one press's actions signed`).toBe(before + actions);
          await expect(owners(page), `${what}: no reactor list`).toHaveCount(0);
          await expect(sheet(page), `${what}: no message sheet`).toBeHidden();
        };
        await step(crab, [[crab, true, "2"], [heart, false, "1"]], 1, "adds");
        await step(heart, [[heart, true, "2"], [crab, false, "1"]], 2, "switches");
        await step(heart, [[heart, false, "1"], [crab, false, "1"]], 1, "takes back");
      });
    }
  });
}

// At 48rem and wider Shift+Enter and Shift+Space open the chip's reactor list
// instead of reacting. A key's click comes on keydown (Enter) or keyup
// (Space); either way it must not react as well, now or once the window in
// which the app swallows it (400 ms) has passed.
test.describe("Shift at 1280px", () => {
  test.beforeEach(async ({ page }) => page.setViewportSize({ width: 1280, height: 800 }));

  for (const key of ["Shift+Enter", "Shift+Space"]) {
    test(`${key} opens the reactor list once and leaves the reaction as it is`, async ({ page }) => {
      const row = await openRoomWithTarget(page);
      await callRiverTest(page, "memberReactsToNewest", "🦀");
      const crab = chip(row, "🦀");
      for (const own of [false, true]) {
        const what = own ? "the reader's chip" : "another member's chip";
        if (own) {
          await crab.click();
          await expectChip(crab, true, "2", "premise: the reader reacted");
          await settle(page);
        }
        const before = await signs(page);
        await crab.press(key);
        await expect(owners(page), `${what}: one list`).toHaveCount(1);
        await expect(owners(page)).toBeFocused();
        await expect(crab).toHaveAttribute("aria-expanded", "true");
        await expect(owners(page).getByTestId("reaction-remove"), `${what}: Remove only on the reader's`).toHaveCount(own ? 1 : 0);
        await expectChip(crab, own, own ? "2" : "1", `${what}: not reacted`);
        await page.waitForTimeout(500);
        await settle(page);
        await expectChip(crab, own, own ? "2" : "1", `${what}: still not reacted past 400 ms`);
        await expect(owners(page), `${what}: still one list`).toHaveCount(1);
        expect(await signs(page), `${what}: nothing was signed`).toBe(before);

        // Escape on the chip closes its own list.
        await crab.focus();
        await page.keyboard.press("Escape");
        await expect(owners(page)).toHaveCount(0);
        await expect(crab).not.toHaveAttribute("aria-expanded");
        await expect(crab).toBeFocused();
      }
      // Nothing of the key's lingers: Enter reacts again, once.
      await crab.press("Enter");
      await expectChip(crab, false, "1", "Enter");
      await settle(page);
      await expectChip(crab, false, "1", "Enter, once");
      await expect(owners(page)).toHaveCount(0);
    });
  }
});

// Order is computed in Rust from the signed actions and pending effects, the
// same in every engine: once, through the real handlers and the DOM.
test.describe("Standing order", { tag: "@chromium-only" }, () => {
  test("appends an alphabetically earlier emoji and moves a removed then re-added interval", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🦀");
    await pick(page, row, "🔥");
    await callRiverTest(page, "memberReactsToNewest", "❤️");
    await expect.poll(() => order(row)).toEqual(["🦀", "🔥", "❤️"]);
    await chip(row, "🔥").click();
    await expect.poll(() => order(row)).toEqual(["🦀", "❤️"]);
    await pick(page, row, "🔥");
    await expect.poll(() => order(row)).toEqual(["🦀", "❤️", "🔥"]);
    // Joining and leaving the older emoji preserves the other member's interval.
    await chip(row, "🦀").click();
    await expect.poll(() => order(row)).toEqual(["🦀", "❤️"]);
    await expect(count(chip(row, "🦀"))).toHaveText("2");
    await chip(row, "🦀").click();
    await expect.poll(() => order(row)).toEqual(["🦀", "❤️"]);
    await expect(count(chip(row, "🦀"))).toHaveText("1");
  });

  test("a pending reaction appends, and keeps its place once signed", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🦀");
    await callRiverTest(page, "memberReactsToNewest", "🔥");
    await expect.poll(() => order(row)).toEqual(["🦀", "🔥"]);
    await callRiverTest(page, "holdMessageSigning");
    await openInlinePicker(page, row);
    await press((await reactionChoices(page)).filter({ hasText: /^❤️$/u }));
    await expect(chip(row, "❤️"), "premise: shown while its signature is held").toHaveCount(1);
    await expect.poll(() => order(row), { message: "pending, it goes last" }).toEqual(["🦀", "🔥", "❤️"]);
    await callRiverTest(page, "releaseMessageSigning");
    await settle(page);
    await expectChip(chip(row, "❤️"), true, "1", "signed");
    expect(await order(row), "signed, it stays last").toEqual(["🦀", "🔥", "❤️"]);
  });
});

// The reader's activation that takes their only reactor off an emoji takes
// its chip away. Focus that was on it stays on the message's reaction line,
// without scrolling; focus that was not is not moved.
test.describe("Chip that leaves with the reader's reaction", () => {
  test.beforeEach(async ({ page }) => page.setViewportSize({ width: 1280, height: 800 }));

  test("hands its focus to another chip, else to React, and only when it had focus", async ({ page }) => {
    const panics = panicsOf(page);
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🦀");
    await callRiverTest(page, "memberReactsToNewest", "❤️");
    await pick(page, row, "👍");
    const thumb = chip(row, "👍");
    await keyboardFocus(page, thumb);
    await page.keyboard.press("Enter");
    await expect(thumb).toHaveCount(0);
    await expect(chips(row), "premise: the other members' chips remain").toHaveCount(2);
    await expect(row.locator('[data-testid="reaction-chip"]:focus'), "focus is on a surviving chip").toHaveCount(1);

    // The message's only chip: focus goes to its React. Arrivals the history
    // follows park the message above the view first, so a focus that
    // scrolled to React would show.
    await callRiverTest(page, "appendMessage", "sole reaction");
    const sole = committedRow(page, "sole reaction");
    await pick(page, sole, "👍");
    await expect(chips(sole), "premise: the message's only chip").toHaveCount(1);
    await keyboardFocus(page, chip(sole, "👍"));
    await callRiverTest(page, "appendMessages", 20);
    await expect(page.getByText("batched arrival 19")).toHaveCount(1, { timeout: 5_000 });
    await settle(page);
    const react = sole.getByTestId("message-react-button");
    await expect(chip(sole, "👍"), "premise: the chip still has focus").toBeFocused();
    expect(await inHistoryView(sole), "premise: the message is out of view").toBe(false);
    expect(await distanceFromEnd(page), "premise: the view is at the history's end").toBeLessThanOrEqual(1);
    // A click with no scroll of its own (Playwright's would scroll to it).
    await chip(sole, "👍").evaluate((el) => (el as HTMLElement).click());
    await expect(chips(sole)).toHaveCount(0);
    await expect(react, "no chip is left: focus is on React").toBeFocused();
    await settle(page);
    expect(await distanceFromEnd(page), "the history did not move").toBeLessThanOrEqual(1);
    expect(await inHistoryView(react), "React is still out of view").toBe(false);

    // A chip without focus leaves focus where it was.
    await callRiverTest(page, "appendMessage", "unfocused reaction");
    const unfocused = committedRow(page, "unfocused reaction");
    await pick(page, unfocused, "👍");
    await settle(page);
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await chip(unfocused, "👍").evaluate((el) => (el as HTMLElement).click());
    await expect(chips(unfocused)).toHaveCount(0);
    await settle(page);
    expect(await page.evaluate(() => document.activeElement === document.body), "focus was not moved").toBe(true);
    expect(panics, "nothing panicked").toEqual([]);
  });
});

// A message's chips are keyed siblings, so a change of standing order moves
// some of them in the DOM. A moved node is detached on the way and loses
// focus: the app puts it back on the chip, or on the open reactor list, the
// move took it from.
test.describe("Standing-order move keeps focus", () => {
  // The reactor list is opened with Shift+Enter, which needs 48rem.
  test.beforeEach(async ({ page }) => page.setViewportSize({ width: 1280, height: 800 }));

  const MOVED = ["❤️", "🎉", "👍"];

  // 👍 (the reader's, earliest), ❤️, 🎉, then another member's later 👍.
  // Removing the reader's 👍 re-ranks it last. With three chips that moves
  // the 👍 itself; a two-chip swap moves the other one.
  async function movableChip(page: Page) {
    const row = await openRoomWithTarget(page);
    await pick(page, row, "👍");
    for (const emoji of ["❤️", "🎉", "👍"]) await callRiverTest(page, "memberReactsToNewest", emoji);
    await expect.poll(() => order(row), { message: "premise: the reader's 👍 stands first" }).toEqual(["👍", "❤️", "🎉"]);
    await expect(count(chip(row, "👍")), "premise: another member holds 👍 too").toHaveText("2");
    return { row, thumb: chip(row, "👍") };
  }

  // The reader takes back their 👍 with Enter on the chip, with signing
  // held: the remove is pending, and a pending remove of an emoji someone
  // else holds moves nothing. The order changes when the caller releases the
  // signature.
  async function removeWithSigningHeld(page: Page, row: Locator, thumb: Locator) {
    await callRiverTest(page, "holdMessageSigning");
    await thumb.press("Enter");
    await expect(thumb, "premise: the remove is showing").toHaveAttribute("aria-pressed", "false");
    await expect(count(thumb), "premise: the other member's 👍 stands").toHaveText("1");
    await expect(thumb, "premise: focus stays on the chip").toBeFocused();
    expect(await order(row), "premise: a pending remove moves nothing").toEqual(["👍", "❤️", "🎉"]);
  }

  // Open the 👍 list with Shift+Enter, still with signing held, and mark its
  // node.
  async function openListWithSigningHeld(page: Page, row: Locator, thumb: Locator) {
    await thumb.press("Shift+Enter");
    await expect(owners(page), "premise: one list is open").toHaveCount(1);
    await expect(owners(page), "premise: the list has focus").toBeFocused();
    await expect(thumb, "premise: the 👍 list").toHaveAttribute("aria-expanded", "true");
    await expect(thumb, "premise: Shift+Enter did not react").toHaveAttribute("aria-pressed", "false");
    expect(await order(row), "premise: opening the list moves nothing").toEqual(["👍", "❤️", "🎉"]);
    await owners(page).evaluate((el) => {
      (el as unknown as { __sameList?: boolean }).__sameList = true;
    });
  }

  const sameList = (page: Page) => owners(page).evaluate((el) => (el as unknown as { __sameList?: boolean }).__sameList === true);

  test("a focused chip the order moves keeps focus, and the history stays put", async ({ page }) => {
    const panics = panicsOf(page);
    const { row, thumb } = await movableChip(page);
    await removeWithSigningHeld(page, row, thumb);
    // Arrivals the history follows park the chip above the view, so a focus
    // that scrolled to it would show.
    await callRiverTest(page, "appendMessages", 20);
    await expect(page.getByText("batched arrival 19")).toHaveCount(1, { timeout: 5_000 });
    await settle(page);
    expect(await inHistoryView(thumb), "premise: the chip is out of view").toBe(false);
    expect(await distanceFromEnd(page), "premise: the view is at the history's end").toBeLessThanOrEqual(1);
    await expect(thumb, "premise: the chip still has focus").toBeFocused();

    await callRiverTest(page, "releaseMessageSigning");
    await expect.poll(() => order(row), { message: "premise: the signed remove moved 👍 last" }).toEqual(MOVED);
    await expect(thumb, "focus stays on the chip the order moved").toBeFocused();
    await settle(page);
    expect(await distanceFromEnd(page), "the history did not move").toBeLessThanOrEqual(1);
    expect(await inHistoryView(thumb), "the chip is still out of view").toBe(false);
    expect(panics, "nothing panicked").toEqual([]);
    // What the two checks above rule out: a focus that scrolls brings the
    // parked chip into view, in every engine.
    await thumb.evaluate((el) => {
      (el as HTMLElement).blur();
      (el as HTMLElement).focus();
    });
    await settle(page);
    expect(await inHistoryView(thumb), "premise: a focus that scrolls would have shown the chip").toBe(true);
  });

  test("an open reactor list the order moves keeps focus, and Escape closes it", async ({ page }) => {
    const panics = panicsOf(page);
    const { row, thumb } = await movableChip(page);
    await removeWithSigningHeld(page, row, thumb);
    await openListWithSigningHeld(page, row, thumb);

    await callRiverTest(page, "releaseMessageSigning");
    await expect.poll(() => order(row), { message: "premise: the signed remove moved 👍 last" }).toEqual(MOVED);
    await expect(owners(page), "premise: the list is still open").toHaveCount(1);
    expect(await sameList(page), "premise: the same list, not one opened again").toBe(true);
    await expect(owners(page), "focus stays in the open list").toBeFocused();
    // From the page, as the reader's key arrives: `owners.press` would focus
    // the list first.
    await page.keyboard.press("Escape");
    await expect(owners(page), "Escape closes the list").toHaveCount(0);
    await expect(thumb, "and focus returns to its chip").toBeFocused();
    expect(panics, "nothing panicked").toEqual([]);
  });

  // In the gateway's frame the chip stays the frame's active element while
  // the reader is in the shell. Another member's reaction must not pull the
  // keyboard back into River.
  test("no focus is taken while the reader is outside the document", async ({ page }) => {
    const panics = panicsOf(page);
    const { row, thumb } = await movableChip(page);
    await removeWithSigningHeld(page, row, thumb);
    // As a browser reports a frame or window the reader has left.
    await page.evaluate(() => Object.defineProperty(document, "hasFocus", { configurable: true, value: () => false }));

    await callRiverTest(page, "releaseMessageSigning");
    await expect.poll(() => order(row), { message: "premise: the signed remove moved 👍 last" }).toEqual(MOVED);
    await settle(page);
    expect(
      await page.evaluate(() => document.activeElement === document.body),
      "the moved chip is not focused again",
    ).toBe(true);
    expect(panics, "nothing panicked").toEqual([]);
  });

  test("focus the reader moved elsewhere is left alone", async ({ page }) => {
    const panics = panicsOf(page);
    const { row, thumb } = await movableChip(page);
    await removeWithSigningHeld(page, row, thumb);
    await openListWithSigningHeld(page, row, thumb);
    const composer = page.getByTestId("message-input");
    await composer.focus();
    await expect(composer, "premise: the reader moved focus to the composer").toBeFocused();
    await expect(owners(page), "premise: the list stayed open").toHaveCount(1);

    await callRiverTest(page, "releaseMessageSigning");
    await expect.poll(() => order(row), { message: "premise: the signed remove moved 👍 last" }).toEqual(MOVED);
    await settle(page);
    await expect(composer, "the composer keeps the focus the reader gave it").toBeFocused();
    await expect(owners(page), "the list stays open").toHaveCount(1);
    expect(await sameList(page), "the same list, not one opened again").toBe(true);
    expect(panics, "nothing panicked").toEqual([]);
  });
});

// A received row's dense footer. An own row's is "Footer at 320px" in
// message-action-buttons.spec.ts, held to the same shared geometry checks.
test("dense received chips wrap inside a 320px footer, and React reaches the row's actions", async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 760 });
  const row = await openRoomWithTarget(page);
  const emojis = ["🦀", "❤️", "👍", "🎉", "🔥", "👀", "🚀", "💯", "😀", "🙌", "✅", "🤔"];
  await callRiverTest(page, "addReactionsToNewest", emojis);
  await expect(chips(row)).toHaveCount(emojis.length);
  await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
  await settle(page);
  const layout = await footerLayout(row);
  expect(layout.left.map((c) => c.id), "every chip, then the one React").toEqual([
    ...Array(emojis.length).fill("reaction-chip"), "message-react-button",
  ]);
  const lines = new Set(layout.left.slice(0, -1).map((c) => Math.round(c.box.top)));
  expect(lines.size, "premise: the chips wrap").toBeGreaterThan(1);
  expect(
    layout.cluster.filter((c) => c.box.width > 0 || c.box.height > 0),
    "no trailing cluster below 48rem",
  ).toEqual([]);
  expectUsableLeftGroup(layout, "received");
  expectClear(layout.left, layout, "received");
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth),
    "the page does not overflow sideways",
  ).toBe(true);
  // Reply is not in this footer. It is one press away: React opens the one
  // message sheet for this row, which holds the reactions and Reply.
  await openSheetFromReact(page, row);
  await expect(palette(page)).toBeVisible();
  await expect(page.getByTestId("mobile-message-action-reply")).toBeVisible();
});
