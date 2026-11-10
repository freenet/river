import { expect, Locator, Page, test } from "@playwright/test";
import { asTouchTablet, coarsePointer, holdChip, keyboardFocus, openInlinePicker, press, reactionChoices, selectRoom, settle, waitForApp, withFingers } from "./example-room";
import { callRiverTest } from "./river-test";

// Regression coverage for freenet/river#714.
//
// Reactor names were rendered only as the chip's native `title`. Desktop
// shows that on hover. A phone never does: there is no hover, and a tap does
// not surface `title` (an Android WebView does not surface it on long-press
// either). The chip of your own reaction removed it on click, so the one
// gesture a phone has destroyed the reaction instead of saying who left it.
//
// The fix added a list of names with an explicit Remove. A press of any kind
// on a chip now reacts with its emoji (a tap, a click, Enter, Space), so the
// list has its own gestures: a finger or pen held on the chip, or
// Shift+Enter. A mouse still has the hover `title`.
//
// CI never caught #714. No test read the names back on a coarse pointer, and
// a `title` attribute can be present while remaining unreachable. These
// tests fail if a held finger does not show the names, if a press opens the
// list instead of reacting, or if the lift of a hold reacts.

function namesFromTitle(title: string): string[] {
  return title
    .replace(/ \(click to remove\)$/, "")
    .split(", ")
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
}

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

// Fixture reactions are written into example state, not sent as signed
// messages. The first real toggle rebuilds that state from messages and
// drops every fixture chip, so "one fewer chip in the room" lands on 0.
// Removal is asserted on a reaction this test adds itself.
async function addOwnReaction(page: Page): Promise<Locator> {
  const row = page
    .locator('[id^="msg-"]')
    .filter({
      hasNot: page.locator('[data-testid="reaction-chip"][title*="click to remove"]'),
    })
    .last();
  await expect(row).toBeVisible();
  const id = await row.getAttribute("id");
  const pinned = page.locator(`[id="${id}"]`);
  await pinned.scrollIntoViewIfNeeded();
  await openInlinePicker(page, pinned);
  await press(page, (await reactionChoices(page)).first());
  const chip = pinned.locator('[data-testid="reaction-chip"][title*="click to remove"]');
  await expect(chip).toHaveCount(1);
  return chip;
}

// Playwright's click() is a mouse pointer even on a touch project, so a
// finger is tap(). Under WebKit, tap()'s click reports pointerType "mouse"
// while its pointerdown says "touch", exactly as iOS Safari 18.2+ does for a
// real finger (WebKit bug 282988). That is why the app decides on the
// pointerdown's type, and why this must stay a real tap(): a synthetic click
// carrying pointerType "touch" feeds in the very value the code branches on
// and passes whether or not that bug is handled.
function mobileProject(): boolean {
  return test.info().project.name.startsWith("mobile");
}

async function pressChip(chip: Locator) {
  if (mobileProject()) {
    await chip.tap();
  } else {
    await chip.click();
  }
}

async function removeOwnReaction(page: Page, chip: Locator) {
  await chip.scrollIntoViewIfNeeded();
  await pressChip(chip);
  await expect(chip).toHaveCount(0);
  await expect(page.getByTestId("reaction-owners"), "a press opens no list").toHaveCount(0);
}

/// A chip another member signed, on the newest message. Unlike a fixture
/// chip it survives the state rebuild a real toggle does.
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
    ? '[data-testid="reaction-chip"][title*="click to remove"]'
    : '[data-testid="reaction-chip"]:not([title*="click to remove"])';
  const chip = page.locator(selector).first();
  await chip.scrollIntoViewIfNeeded();
  await expect(chip).toBeVisible();
  return chip;
}

// The reactor list exists at 48rem and wider: below it there is none, and the
// mobile message sheet lists who reacted (mobile-reaction-popup.spec.ts).
// So a phone project runs these at a touch tablet's width.
test.beforeEach(async ({ page }) => asTouchTablet(page));

test.describe("Reaction owner list (#714)", () => {
  test("a press reacts with any pointer; a held finger reads who reacted", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);

    const own = await chipByOwnReaction(page, true);
    const other = await chipByOwnReaction(page, false);
    const ownTitle = (await own.getAttribute("title")) ?? "";
    const otherTitle = (await other.getAttribute("title")) ?? "";
    expect(namesFromTitle(ownTitle)).toContain("You");
    expect(namesFromTitle(otherTitle).length).toBeGreaterThan(0);

    const owners = page.getByTestId("reaction-owners");
    const coarse = await coarsePointer(page);
    // A mobile project that reports a fine pointer must not silently run the
    // desktop half and pass (freenet/river#714 review).
    expect(coarse).toBe(test.info().project.name.startsWith("mobile"));

    // Reading, on fixture chips. A held finger opens the list; a mouse has
    // the title, checked above.
    if (coarse) {
      const chipHeight = (await other.boundingBox())?.height ?? 0;

      let lift = await holdChip(page, other);
      await expect(owners).toBeVisible();
      await expect(owners.getByTestId("reaction-remove")).toHaveCount(0);
      await expect(owners.getByTestId("reaction-owner")).toHaveCount(
        namesFromTitle(otherTitle).length
      );
      for (const name of namesFromTitle(otherTitle)) {
        await expect(
          owners.getByTestId("reaction-owner").filter({ hasText: exactName(name) })
        ).toHaveCount(1);
      }
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

      lift = await holdChip(page, own);
      await expect(owners).toBeVisible();
      await expect(
        owners.getByTestId("reaction-owner").filter({ hasText: exactName("You") })
      ).toHaveCount(1);
      await expect(owners.getByTestId("reaction-remove")).toBeVisible();
      await lift();
      await page.mouse.click(8, 8);
      await expect(owners).toHaveCount(0);
    }

    // Pressing, on a signed chip: a tap on a touch project, a click otherwise.
    const crab = await signedChip(page);
    await pressChip(crab);
    await expect(crab, "someone else's chip takes the reader's reaction").toContainText("2");
    await expect(crab).toHaveAttribute("title", /click to remove/);
    await expect(owners, "a press opens no list").toHaveCount(0);
    await pressChip(crab);
    await expect(crab, "and gives it back").toContainText("1");
    await expect(owners).toHaveCount(0);

    const added = await addOwnReaction(page);
    await removeOwnReaction(page, added);
  });
});

test.describe("Reaction chip held (#714)", () => {
  test("a held finger opens the list; lifting it neither closes the list nor reacts", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);

    const owners = page.getByTestId("reaction-owners");
    const ownSelector = '[data-testid="reaction-chip"][title*="click to remove"]';
    const own = await chipByOwnReaction(page, true);
    const ownCount = await page.locator(ownSelector).count();
    const names = namesFromTitle((await own.getAttribute("title")) ?? "");

    const lift = await holdChip(page, own);
    // Before the lift: nothing but the hold can have opened it.
    await expect(owners).toBeVisible();
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await expect(owners.getByTestId("reaction-owner")).toHaveCount(names.length);
    await lift();
    await settle(page);
    await expect(owners, "the lift's click does not close it").toBeVisible();
    await expect(page.locator(ownSelector), "nor remove the reaction").toHaveCount(ownCount);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);

    // A resting finger selects nothing and raises no callout.
    expect(
      await own.evaluate((el) => {
        const s = getComputedStyle(el) as CSSStyleDeclaration & { webkitUserSelect?: string };
        return s.userSelect || s.webkitUserSelect;
      }),
    ).toBe("none");
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
    await press(page, (await reactionChoices(page)).filter({ hasNotText: "🦀" }).first());
    const own = message.locator('[data-testid="reaction-chip"][title*="click to remove"]');
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
  test("Enter and Space react; Shift+Enter opens the list, and closing it does not scroll the history", async ({
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

    // Your own reaction: Shift+Enter opens the list, with Remove in it, and
    // does not remove the reaction the way Enter alone does.
    const own = await chipByOwnReaction(page, true);
    const ownCount = await page
      .locator('[data-testid="reaction-chip"][title*="click to remove"]')
      .count();
    await own.focus();
    await own.press("Shift+Enter");
    await expect(owners).toBeVisible();
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await expect(owners).toBeFocused();
    await expect(
      page.locator('[data-testid="reaction-chip"][title*="click to remove"]')
    ).toHaveCount(ownCount);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);
    await expect(own).toBeFocused();

    // Someone else's too. Then move the history so the chip is out of view while the
    // list is open: closing it puts focus back on the chip, and that must
    // not scroll the history back to it (only an arrival moves the view).
    const other = await chipByOwnReaction(page, false);
    await other.focus();
    await other.press("Shift+Enter");
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

    // Without Shift, Enter and Space react, and focus stays on the chip.
    const crab = await signedChip(page);
    // The room's first real toggle rebuilds its state from signed messages,
    // which drops the fixture chips and re-mounts the rows. Make it here,
    // before focus is on the chip.
    await pressChip(crab);
    await expect(crab).toContainText("2");
    await pressChip(crab);
    await expect(crab).toContainText("1");
    await settle(page);
    await keyboardFocus(page, crab);
    await page.keyboard.press("Enter");
    await expect(crab).toContainText("2");
    await expect(owners).toHaveCount(0);
    await expect(crab).toBeFocused();
    await page.keyboard.press("Space");
    await expect(crab).toContainText("1");

    // The reader's only chip leaves with the reaction: focus goes to the +.
    const sole = await addOwnReaction(page);
    const messageId = await sole.evaluate((el) => el.closest('[id^="msg-"]')!.id);
    await keyboardFocus(page, sole);
    await page.keyboard.press("Enter");
    await expect(sole).toHaveCount(0);
    await expect(page.locator(`[id="${messageId}"]`).getByTestId("message-react-button")).toBeFocused();
  });
});

test.describe("Reaction chip activated without a press", () => {
  test("a click with no press of its own reacts, as a press does", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);

    const owners = page.getByTestId("reaction-owners");
    const crab = await signedChip(page);
    // What assistive technology does: a click event with no pointerdown.
    await crab.evaluate((el) => (el as HTMLElement).click());
    await expect(crab).toContainText("2");
    await expect(owners).toHaveCount(0);
    await crab.evaluate((el) => (el as HTMLElement).click());
    await expect(crab).toContainText("1");
  });
});

test.describe("Touchscreen with a mouse (#714)", { tag: "@chromium-only" }, () => {
  test.use({ hasTouch: true, viewport: { width: 1280, height: 800 } });

  test("a mouse click and a tap both react; a real held finger opens the list", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    expect(await coarsePointer(page)).toBe(true);
    const owners = page.getByTestId("reaction-owners");

    const clicked = await addOwnReaction(page);
    await clicked.click();
    await expect(clicked).toHaveCount(0);
    await expect(owners).toHaveCount(0);

    const tapped = await addOwnReaction(page);
    await tapped.tap();
    await expect(tapped).toHaveCount(0);
    await expect(owners).toHaveCount(0);

    // A real finger, through Chromium's input protocol: it rests on the
    // chip, the list opens, and the click its lift sends is not a press.
    const held = await addOwnReaction(page);
    const box = (await held.boundingBox())!;
    await withFingers(page, async (fingers) => {
      await fingers.press([1, Math.round(box.x + box.width / 2), Math.round(box.y + box.height / 2)]);
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
  // beforeEach); mobile-reaction-popup.spec.ts has the mobile sheet's.
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
    // Park it again, now that the reaction is in. This first real toggle
    // rebuilds the room and drops every fixture chip, and the history keeps
    // the reading row in place across that, not this message, so where the
    // message ends up depends on the random fixture data: in about 1 run in
    // 200 its React button ended 5px below the view. Playwright's tap() and
    // focus() then scroll it into view themselves, and "the history did not
    // move" failed on a scroll the app never made.
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

    await chip.press("Shift+Enter");
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
    await page.keyboard.press("Shift+Enter");
    await expect.poll(() => surfaces(page)).toEqual({ picker: [], pickerOpen: false, owners: [id] });
    await expect(page.getByTestId("reaction-owners")).toBeFocused();
    await settle(page);
    expect(await historyScrollTop(page), "the history did not move").toBeCloseTo(parked, 0);
  });

  test("by touch", async ({ page }) => {
    test.skip(!mobileProject(), "a held finger, on a touch project");
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
