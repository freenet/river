import { expect, Locator, Page, test } from "@playwright/test";
import {
  asTouchTablet,
  centreOf,
  chips,
  coarsePointer,
  committedRow,
  hasTouch,
  holdChip,
  openInlinePicker,
  openRoomWithComposer,
  openRoomWithTarget,
  openSheetFromReact,
  palette,
  press,
  reactionChoices,
  selectRoom,
  settle,
  sheet,
  waitForApp,
  withFingers,
} from "./example-room";
import { callRiverTest } from "./river-test";

// Regression coverage for freenet/river#714.
//
// Reactor names were rendered only as the chip's native `title`. Desktop
// shows that on hover. A phone never does: there is no hover, and a tap does
// not surface `title` (an Android WebView does not surface it on long-press
// either). The chip of your own reaction removed it on click, so the one
// gesture a phone has destroyed the reaction instead of saying who left it.
//
// Native chips expose the reactor list to touch and keyboard activation.
// Mouse/fine-pen activation toggles directly. A held finger remains an
// inspection shortcut and its eventual click must never apply a reaction.

// Substring matching would treat the row "You" as a hit inside "Young".
function exactName(name: string): RegExp {
  return new RegExp(`^${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`);
}

// Room entry scrolls to the newest message asynchronously. Wait until that
// settles, then scroll a chip into view, or the entry scroll snaps the chip
// away under the click (same race as mobile-touch-ux.spec.ts).
async function waitSettledAtBottom(page: Page) {
  await expect
    .poll(
      () =>
        page.evaluate(() => {
          const el = document.getElementById("chat-scroll-container");
          if (!el) return Number.MAX_SAFE_INTEGER;
          return el.scrollHeight - el.scrollTop - el.clientHeight;
        }),
      { timeout: 5_000 }
    )
    .toBeLessThan(120);
}

// Use a reaction this test adds for removal and disappearing-chip focus.
async function addOwnReaction(page: Page): Promise<Locator> {
  const row = page
    .locator('[id^="msg-"]')
    .filter({
      hasNot: page.getByTestId("reaction-chip"),
    })
    .last();
  await expect(row).toBeVisible();
  const id = await row.getAttribute("id");
  const pinned = page.locator(`[id="${id}"]`);
  await pinned.scrollIntoViewIfNeeded();
  await openInlinePicker(page, pinned);
  await press((await reactionChoices(page)).first());
  const chip = pinned.locator('[data-testid="reaction-chip"][aria-pressed="true"]');
  await expect(chip).toHaveCount(1);
  return chip;
}

// Playwright's click() is a mouse pointer even on a touch project, so a
// finger is tap(). Under WebKit, tap()'s click reports pointerType "mouse"
// while its pointerdown says "touch", exactly as iOS Safari 18.2+ does for a
// real finger (WebKit bug 282988). That is why the app decides on the
// pointerdown's type, and why this must stay a real tap(): a synthetic click
// carrying pointerType "touch" feeds in the very value the code branches on
// and passes whether or not that bug is handled. `press` is that tap.

// Remove the reader's reaction through `chip`'s list, opened from the
// keyboard. The caller asserts what its case is about: the chip going, or a
// shared chip staying with one reactor fewer.
async function removeFromList(page: Page, chip: Locator) {
  await chip.scrollIntoViewIfNeeded();
  await chip.press("Enter");
  const owners = page.getByTestId("reaction-owners");
  await expect(owners).toBeVisible();
  await owners.getByTestId("reaction-remove").click();
  await expect(owners).toHaveCount(0);
}

/// A known reactor on the newest message, signed through the action path.
async function signedChip(page: Page): Promise<Locator> {
  await callRiverTest(page, "memberReactsToNewest", "🦀");
  const row = page
    .locator('[id^="msg-"]')
    .filter({ has: page.getByTestId("reaction-chip").filter({ hasText: "🦀" }) })
    .last();
  const id = await row.getAttribute("id");
  const chip = page.locator(`[id="${id}"]`).getByTestId("reaction-chip").filter({ hasText: "🦀" });
  await chip.scrollIntoViewIfNeeded();
  await expect(chip).toContainText("1");
  return chip;
}

/// Scroll the history so `chip` is just out of view, without leaving the
/// rendered range.
///
/// The example data is random, so where the chip sits is too. A chip about
/// one view down from the top of a short history has neither room above it
/// (to park it below the view) nor history enough below it (to park it
/// above): the scroll clamped at the end with the chip still on screen, and
/// the premise failed about once a CI run (#823 review). Callers give the
/// history room first (`appendMessages`); the placement chosen here is
/// checked to be possible rather than clamped, and says so if not.
async function scrollChipOutOfView(page: Page, chip: Locator): Promise<void> {
  const plan = await chip.evaluate((el) => {
    const scroller = document.getElementById("chat-scroll-container")!;
    const chipRect = el.getBoundingClientRect();
    const viewRect = scroller.getBoundingClientRect();
    const chipTop = chipRect.top - viewRect.top + scroller.scrollTop;
    const chipBottom = chipTop + chipRect.height;
    const max = scroller.scrollHeight - scroller.clientHeight;
    // Put the chip just below the view if there is room above it, else just
    // above the view.
    const below = chipTop - scroller.clientHeight - 24;
    const target = below >= 0 ? below : chipBottom + 24;
    if (target <= max) scroller.scrollTop = target;
    return { chipTop, chipBottom, max, client: scroller.clientHeight, target };
  });
  expect(plan.target, `premise: the history has room to move the chip out of view ${JSON.stringify(plan)}`).toBeLessThanOrEqual(plan.max);
  await settle(page);
  const inView = await chip.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const v = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return r.bottom > v.top && r.top < v.bottom;
  });
  expect(inView, `premise: the chip is scrolled out of view ${JSON.stringify(plan)}`).toBe(false);
}

function historyScrollTop(page: Page): Promise<number> {
  return page.evaluate(() => document.getElementById("chat-scroll-container")!.scrollTop);
}

async function chipByOwnReaction(page: Page, own: boolean): Promise<Locator> {
  const selector = own
    ? '[data-testid="reaction-chip"][aria-pressed="true"]'
    : '[data-testid="reaction-chip"][aria-pressed="false"]';
  const chip = page.locator(selector).first();
  await chip.scrollIntoViewIfNeeded();
  await expect(chip).toBeVisible();
  return chip;
}

// Run the shared tablet/desktop inspection and handoff cases at 48rem or wider.
// The narrow cases below keep each project's actual pointer input.
test.beforeEach(async ({ page }) => asTouchTablet(page));

test.describe("Reaction owner list (#714)", () => {
  test("a held finger reads who reacted, and its lift neither closes the list nor reacts", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);

    const own = await chipByOwnReaction(page, true);
    const other = await chipByOwnReaction(page, false);
    const ownReactors = Number(await own.getByTestId("reaction-chip-count").innerText());
    const otherReactors = Number(await other.getByTestId("reaction-chip-count").innerText());
    const owners = page.getByTestId("reaction-owners");
    const coarse = await coarsePointer(page);
    // A mobile project that reports a fine pointer must not silently run the
    // desktop half and pass (freenet/river#714 review).
    expect(coarse).toBe(hasTouch());

    // Reading, on fixture chips: a held finger opens the list before lift.
    if (coarse) {
      const chipHeight = (await other.boundingBox())?.height ?? 0;

      const lift = await holdChip(page, other);
      await expect(owners).toBeVisible();
      await expect(owners.getByTestId("reaction-remove")).toHaveCount(0);
      await expect(owners.getByTestId("reaction-owner")).toHaveCount(
        otherReactors
      );
      await expect(owners.getByTestId("reaction-owner").filter({ hasText: exactName("You") })).toHaveCount(0);
      // The list is absolutely positioned. Opening it must not grow the chip,
      // which is the failure mode that got the enlarged "+" target reverted
      // (#605): a taller reaction row on every message.
      const heightAfter = (await other.boundingBox())?.height ?? 0;
      expect(heightAfter).toBeCloseTo(chipHeight, 0);
      await expectOnScreen(page, owners);
      await lift();

      // The backdrop covers the rest of the history, so this tap dismisses
      // and does not leave a second popover open.
      await page.mouse.click(8, 8);
      await expect(owners).toHaveCount(0);
      await expect(page.getByTestId("emoji-picker")).toBeHidden();
    }

    // The reader's own chip, held. On every project: the app decides a hold
    // on the pointerdown's type, and `holdChip` sends a touch.
    const ownSelector = '[data-testid="reaction-chip"][aria-pressed="true"]';
    const ownCount = await page.locator(ownSelector).count();
    const lift = await holdChip(page, own);
    // Before the lift: nothing but the hold can have opened it.
    await expect(owners).toBeVisible();
    await expect(owners.getByTestId("reaction-owner")).toHaveCount(ownReactors);
    await expect(
      owners.getByTestId("reaction-owner").filter({ hasText: exactName("You") })
    ).toHaveCount(1);
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await lift();
    await settle(page);
    await expect(owners, "the lift's click does not close it").toBeVisible();
    await expect(page.locator(ownSelector), "nor remove the reaction").toHaveCount(ownCount);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);

    // A real short tap at this width starts the hold's timer and lifts
    // before it runs out: it opens the same list without applying, and the
    // timer opens nothing afterwards. Below 48rem no timer starts ("Reaction
    // chip below 48rem"); a mouse press is reaction-chips.spec.ts's.
    if (hasTouch()) {
      const crab = await signedChip(page);
      await crab.tap();
      await expect(owners).toBeVisible();
      await expect(owners.getByTestId("reaction-apply")).toHaveText("React with 🦀");
      await expect(crab).toHaveAttribute("aria-pressed", "false");
      await expect(crab.getByTestId("reaction-chip-count")).toHaveText("1");
      await owners.press("Escape");
      await expect(owners).toHaveCount(0);
      // Past CHIP_HOLD_MS (500).
      await page.waitForTimeout(700);
      await expect(owners, "a short tap is not a hold").toHaveCount(0);
      await expect(crab).toHaveAttribute("aria-pressed", "false");
    }
  });
});

// Removing your reaction from the list while another member's chip remains
// leaves focus on that chip, not on the React action at the message's top.
test.describe("Reaction removed from the owner list", () => {
  test("focus stays on the reaction line while a chip remains", async ({ page }) => {
    test.skip(!(await coarsePointer(page)), "a held finger opens the list on a coarse pointer");
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await settle(page);
    await callRiverTest(page, "memberReactsToNewest", "🦀");
    const target = page.locator('[id^="msg-"]').filter({ has: page.getByTestId("reaction-chip").filter({ hasText: "🦀" }) });
    await expect(target).toHaveCount(1);
    const message = page.locator(`[id="${await target.getAttribute("id")}"]`);
    const chips = message.getByTestId("reaction-chip");
    await message.scrollIntoViewIfNeeded();
    await openInlinePicker(page, message);
    await press((await reactionChoices(page)).filter({ hasNotText: "🦀" }).first());
    const own = message.locator('[data-testid="reaction-chip"][aria-pressed="true"]');
    await expect(own).toHaveCount(1);
    await expect(chips).toHaveCount(2);

    const lift = await holdChip(page, own);
    const owners = page.getByTestId("reaction-owners");
    await expect(owners).toBeVisible();
    await lift();
    await owners.getByTestId("reaction-remove").click();
    await expect(own).toHaveCount(0);
    await expect(chips).toHaveCount(1);
    await expect(chips.first()).toBeFocused();
  });
});

// Removing the message's last reaction from the list leaves no chip to go back
// to: focus goes to the message's footer React, which is always there, and
// without moving the history (only an arrival moves the view).
test.describe("Last reaction removed from the owner list", () => {
  test("focus goes to the footer React and the history stays put", async ({ page }) => {
    test.skip(!(await coarsePointer(page)), "a held finger opens the list on a coarse pointer");
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    const chip = await addOwnReaction(page);
    const messageId = await chip.evaluate((el) => el.closest('[id^="msg-"]')!.id);
    const message = page.locator(`[id="${messageId}"]`);
    await expect(message.getByTestId("reaction-chip"), "premise: the message's only chip").toHaveCount(1);

    const lift = await holdChip(page, chip);
    const owners = page.getByTestId("reaction-owners");
    await expect(owners).toBeVisible();
    await lift();
    // Park the message out of view, so a focus that scrolled would show.
    await scrollChipOutOfView(page, chip);
    const parked = await historyScrollTop(page);
    // A click with no scroll of its own (Playwright's would scroll to it).
    await owners.getByTestId("reaction-remove").evaluate((el) => (el as HTMLElement).click());
    await expect(chip).toHaveCount(0);
    await expect(owners).toHaveCount(0);
    await expect(message.getByTestId("message-react-button")).toBeFocused();
    await settle(page);
    expect(await historyScrollTop(page), "the history did not move").toBeCloseTo(parked, 0);
  });
});

test.describe("Reaction chip from the keyboard (#714)", () => {
  // Enter and Space each inspecting once, with nothing toggled, is
  // reaction-chips.spec.ts's ("inspection exposes explicit React, Switch and
  // Remove actions"), on every project.
  test("closing the list does not scroll the history, and removing the last chip moves focus to React", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    // History below every fixture chip, so the chip can be parked above the
    // view whatever the random example data put around it.
    await callRiverTest(page, "appendMessages", 20);
    await expect(page.getByText("batched arrival 19")).toHaveCount(1, { timeout: 5_000 });
    await waitSettledAtBottom(page);

    const owners = page.getByTestId("reaction-owners");

    // Your own reaction: native Enter opens the list with Remove.
    const own = await chipByOwnReaction(page, true);
    const ownCount = await page
      .locator('[data-testid="reaction-chip"][aria-pressed="true"]')
      .count();
    await own.focus();
    await own.press("Enter");
    await expect(owners).toBeVisible();
    await expect(own, "the row's band keys on it").toHaveAttribute("aria-expanded", "true");
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await expect(owners).toBeFocused();
    await expect(
      page.locator('[data-testid="reaction-chip"][aria-pressed="true"]')
    ).toHaveCount(ownCount);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);
    await expect(own).toBeFocused();

    // Someone else's too. Then move the history so the chip is out of view while the
    // list is open: closing it puts focus back on the chip, and that must
    // not scroll the history back to it (only an arrival moves the view).
    const other = await chipByOwnReaction(page, false);
    await other.focus();
    await other.press("Enter");
    await expect(owners).toBeVisible();
    await expect(owners).toBeFocused();
    await scrollChipOutOfView(page, other);
    // Read after the scroll has settled, not before.
    const parked = await historyScrollTop(page);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);
    await expect(other).toBeFocused();
    await settle(page);
    expect(await historyScrollTop(page)).toBeCloseTo(parked, 0);

    // Removal is explicit, including when the last chip disappears.
    const sole = await addOwnReaction(page);
    const messageId = await sole.evaluate((el) => el.closest('[id^="msg-"]')!.id);
    await removeFromList(page, sole);
    await expect(sole).toHaveCount(0);
    await expect(page.locator(`[id="${messageId}"]`).getByTestId("message-react-button")).toBeFocused();
  });
});

test.describe("Reaction chip under a real finger (#714)", { tag: "@chromium-only" }, () => {
  test.use({ hasTouch: true, viewport: { width: 1280, height: 800 } });

  test("a real held finger opens the list, and its lift neither closes it nor reacts", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    expect(await coarsePointer(page)).toBe(true);
    const owners = page.getByTestId("reaction-owners");

    // Through Chromium's input protocol: the finger rests on the chip, the
    // list opens, and the click its lift sends is not a press.
    const held = await addOwnReaction(page);
    const [x, y] = await centreOf(held);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await expect(owners).toBeVisible();
      await fingers.lift();
    });
    await settle(page);
    await expect(owners, "the lift does not close it").toBeVisible();
    await expect(held, "nor remove the reaction").toHaveCount(1);
  });
});

async function expectOnScreen(page: Page, target: Locator) {
  const box = await target.boundingBox();
  const vp = page.viewportSize();
  expect(box).not.toBeNull();
  if (box && vp) {
    expect(box.width).toBeGreaterThan(0);
    expect(box.height).toBeGreaterThan(0);
    expect(box.x).toBeGreaterThanOrEqual(-1);
    expect(box.y).toBeGreaterThanOrEqual(-1);
    expect(box.x + box.width).toBeLessThanOrEqual(vp.width + 1);
    expect(box.y + box.height).toBeLessThanOrEqual(vp.height + 1);
  }
}

// The reactor list and the reaction picker are separate surfaces, and at most
// one of them is open: opening either closes the other, from the keyboard or
// by touch, on the message the reader chose, without stealing focus back or
// moving the history.
test.describe("Reactor list and picker hand off", () => {
  // The anchored picker's hand-off, at a touch tablet's width (the file's
  // beforeEach); mobile-message-sheet.spec.ts has the mobile sheet's.
  async function surfaces(page: Page) {
    return page.evaluate(() => ({
      picker: Array.from(document.querySelectorAll('[data-testid="message-react-button"][aria-expanded="true"]')).map(
        (b) => b.closest('[id^="msg-"]')!.id,
      ),
      pickerOpen: !!document.querySelector('[data-testid="emoji-picker"]')?.matches(":popover-open"),
      owners: Array.from(document.querySelectorAll('[data-testid="reaction-owners"]')).map(
        (l) => l.closest('[id^="msg-"]')!.id,
      ),
    }));
  }

  /// A message of the reader's with one own reaction, parked short of the end.
  async function reactedMessage(page: Page): Promise<{ message: Locator; id: string; chip: Locator }> {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await callRiverTest(page, "appendMessages", 6);
    await expect(page.getByText("batched arrival 05")).toHaveCount(1, { timeout: 5_000 });
    await waitSettledAtBottom(page);
    const target = page.locator('[id^="msg-"]').filter({ hasText: "batched arrival 03" });
    const id = (await target.getAttribute("id"))!;
    const message = page.locator(`[id="${id}"]`);
    await message.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await openInlinePicker(page, message);
    await page.getByTestId("emoji-picker").getByRole("button").first().click();
    const chip = message.getByTestId("reaction-chip");
    await expect(chip).toHaveCount(1);
    await settle(page);
    // Park it again after the footer gains its reaction line.
    await message.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await settle(page);
    await expectInHistoryView(page, message.getByTestId("message-react-button"));
    await expectInHistoryView(page, chip);
    return { message, id, chip };
  }

  /// `target` lies wholly inside the history's view, so nothing acting on it
  /// has to scroll it there first.
  async function expectInHistoryView(page: Page, target: Locator) {
    const [box, view] = await Promise.all([
      target.boundingBox(),
      page.locator("#chat-scroll-container").boundingBox(),
    ]);
    expect(box).not.toBeNull();
    expect(view).not.toBeNull();
    expect(box!.y, "below the history's top edge").toBeGreaterThanOrEqual(view!.y);
    expect(box!.y + box!.height, "above the history's bottom edge").toBeLessThanOrEqual(view!.y + view!.height);
  }

  test("from the keyboard", async ({ page }) => {
    const { message, id, chip } = await reactedMessage(page);
    const react = message.getByTestId("message-react-button");
    await chip.focus();
    const parked = await historyScrollTop(page);

    await chip.press("Enter");
    await expect(page.getByTestId("reaction-owners")).toBeFocused();
    expect(await surfaces(page)).toEqual({ picker: [], pickerOpen: false, owners: [id] });

    // List to picker: the list closes, and its focus return does not take
    // focus back from React.
    await react.focus();
    await page.keyboard.press("Enter");
    await expect.poll(() => surfaces(page)).toEqual({ picker: [id], pickerOpen: true, owners: [] });
    await settle(page);
    await expect(react, "focus stays on React").toBeFocused();

    // Picker to list.
    await chip.focus();
    await page.keyboard.press("Enter");
    await expect.poll(() => surfaces(page)).toEqual({ picker: [], pickerOpen: false, owners: [id] });
    await expect(page.getByTestId("reaction-owners")).toBeFocused();
    await settle(page);
    expect(await historyScrollTop(page), "the history did not move").toBeCloseTo(parked, 0);
  });

  test("by touch", async ({ page }) => {
    test.skip(!hasTouch(), "a held finger, on a touch project");
    const { message, id, chip } = await reactedMessage(page);
    const react = message.getByTestId("message-react-button");
    const parked = await historyScrollTop(page);

    // Picker to list: a held finger on the chip closes the picker and opens
    // the list.
    await react.tap();
    await expect.poll(() => surfaces(page)).toEqual({ picker: [id], pickerOpen: true, owners: [] });
    const lift = await holdChip(page, chip);
    await expect.poll(() => surfaces(page)).toEqual({ picker: [], pickerOpen: false, owners: [id] });
    await lift();

    // List to picker: the list's backdrop takes the first tap, which only
    // closes it, as it always has; the next opens the picker.
    const box = (await react.boundingBox())!;
    await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
    await expect.poll(() => surfaces(page)).toEqual({ picker: [], pickerOpen: false, owners: [] });
    await react.tap();
    await expect.poll(() => surfaces(page)).toEqual({ picker: [id], pickerOpen: true, owners: [] });
    await settle(page);
    expect(await historyScrollTop(page), "the history did not move").toBeCloseTo(parked, 0);
  });
});

// Chips stay excluded from row taps at phone widths. Inspection exposes
// explicit actions without opening the mobile message sheet.
test.describe("Reaction chip below 48rem", () => {
  test.beforeEach(async ({ page }) => page.setViewportSize({ width: 390, height: 760 }));

  test("touch and keyboard inspect; explicit React, Switch and Remove keep the sheet closed", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    const party = row.locator('[data-testid="reaction-chip"][data-emoji="🎉"]');
    const owners = page.getByTestId("reaction-owners");
    await expect(party.getByTestId("reaction-chip-count")).toHaveText("1");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));

    // Real touch exercises WebKit's touch-click-as-mouse behavior. Desktop
    // projects inspect from the keyboard because their mouse applies directly.
    if (hasTouch()) await party.tap();
    else await party.press("Enter");
    await expect(owners).toBeVisible();
    await expect(party).toHaveAttribute("aria-pressed", "false");
    await expect(sheet(page), "a chip never opens the sheet").toBeHidden();
    await expect(owners.getByTestId("reaction-apply")).toHaveText("React with 🎉");
    await owners.getByTestId("reaction-apply").click();
    await expect(party.getByTestId("reaction-chip-count")).toHaveText("2");
    await expect(party).toHaveAttribute("aria-pressed", "true");
    await removeFromList(page, party);
    await expect(party, "the shared chip stays").toHaveAttribute("aria-pressed", "false");
    await expect(party.getByTestId("reaction-chip-count")).toHaveText("1");

    // React with a different emoji through the production picker, then the
    // explicit list action switches it to the standing emoji.
    await openSheetFromReact(page, row);
    await press(palette(page).getByRole("button", { name: "React with 👍" }));
    await expect(chips(row)).toHaveCount(2);
    await party.press("Space");
    await expect(owners.getByTestId("reaction-apply")).toHaveText("Switch to 🎉");
    await owners.getByTestId("reaction-apply").click();
    await expect(chips(row), "the 👍 left with the switch").toHaveCount(1);
    await expect(party).toHaveAttribute("aria-pressed", "true");
    await expect(sheet(page)).toBeHidden();
  });
});

// A continuation's chip keeps its id when its group's first message leaves
// and the group remounts. A hold begun on the old chip must not open the new
// group's list, or write to the old group's state.
test.describe("Reaction chip held while its group remounts", { tag: "@chromium-only" }, () => {
  test("the hold opens nothing, nothing panics, and the remounted chip remains usable", async ({ page }) => {
    const panics: string[] = [];
    page.on("pageerror", (e) => panics.push(e.message));
    page.on("console", (m) => {
      if (m.type() === "error" && /panicked|unreachable/.test(m.text())) panics.push(m.text());
    });
    await openRoomWithComposer(page);
    await callRiverTest(page, "deliverTimedMessages", 0, [
      { text: "hold lead", sentSecs: 1, receivedSecs: 0 },
      { text: "hold survivor", sentSecs: 2, receivedSecs: 0 },
    ]);
    const survivor = committedRow(page, "hold survivor");
    await expect(survivor).toHaveCount(1);
    await callRiverTest(page, "memberReactsToNewest", "🦀");
    const chip = chips(survivor).filter({ hasText: "🦀" });
    await expect(chip).toContainText("1");
    await chip.scrollIntoViewIfNeeded();
    // A finger goes down on the continuation's chip, then the group's first
    // message leaves: the group remounts with a new chip under the same id.
    await chip.evaluate((el) => {
      const w = window as unknown as { __oldChip: Element; __riverTest: { removeMessages(text: string): void } };
      w.__oldChip = el;
      const r = el.getBoundingClientRect();
      el.dispatchEvent(
        new PointerEvent("pointerdown", {
          bubbles: true,
          cancelable: true,
          composed: true,
          pointerId: 7,
          pointerType: "touch",
          isPrimary: true,
          clientX: r.left + r.width / 2,
          clientY: r.top + r.height / 2,
        }),
      );
      w.__riverTest.removeMessages("hold lead");
    });
    await expect(committedRow(page, "hold lead")).toHaveCount(0);
    expect(
      await chip.evaluate((el) => el === (window as unknown as { __oldChip: Element }).__oldChip),
      "premise: the chip remounted",
    ).toBe(false);
    // Past CHIP_HOLD_MS (500).
    await page.waitForTimeout(700);
    expect(panics, "the hold's timer did not panic").toEqual([]);
    await expect(page.getByTestId("reaction-owners"), "no finger is on the new chip").toHaveCount(0);
    await chip.press("Enter");
    await expect(page.getByTestId("reaction-owners")).toBeVisible();
    await expect(chip.getByTestId("reaction-chip-count")).toHaveText("1");
  });
});

test.describe("Reactor list across 48rem", () => {
  test("inspection remains available when narrowing below 48rem", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    const chip = await signedChip(page);
    const owners = page.getByTestId("reaction-owners");
    await chip.press("Enter");
    await expect(owners).toBeVisible();
    const { width, height } = page.viewportSize()!;
    await page.setViewportSize({ width: 390, height });
    await expect(owners, "the list remains available on a phone").toBeVisible();
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);
    await page.setViewportSize({ width, height });
    await settle(page);
    await expect(owners, "nothing reopens it").toHaveCount(0);
  });
});
