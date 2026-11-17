import { expect, Locator, Page, test } from "@playwright/test";
import {
  asTouchTablet,
  centreOf,
  chipFor,
  chipPointer,
  chips,
  coarsePointer,
  committedRow,
  expectSheetOpenFor,
  hasTouch,
  holdChip,
  narrowLayout,
  openInlinePicker,
  openRoomWithComposer,
  openRoomWithTarget,
  openSheetFromReact,
  OWN_CHIP,
  palette,
  plainPointOf,
  press,
  pressAt,
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
  const chip = pinned.locator(OWN_CHIP);
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
async function signedChip(page: Page, nickname?: string): Promise<Locator> {
  if (nickname === undefined) await callRiverTest(page, "memberReactsToNewest", "🦀");
  else await callRiverTest(page, "memberReactsToNewest", "🦀", nickname);
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
  const selector = own ? OWN_CHIP : '[data-testid="reaction-chip"][data-own-reaction="false"]';
  const chip = page.locator(selector).first();
  await chip.scrollIntoViewIfNeeded();
  await expect(chip).toBeVisible();
  return chip;
}

/// The lift of the finger `holdChip` put down on `chip`, as one page task:
/// `pointerup` and the `pointerleave` a touch sends next, both on the chip,
/// then the lift's click, sent to whatever the chip's centre hit-tests to.
/// By then that is the list's backdrop, which covers the chip, and the app
/// takes the lift's click there as it does on the chip. One task, so the
/// app's release window (CHIP_HOLD_RELEASE_MS, 400) cannot run out between
/// the lift and its click. The click goes out only if the hit is that
/// backdrop, never to the chip instead: the caller asserts on the hit it
/// gets back.
///
/// A dispatched click on purpose. A tap or a mouse click begins a press of
/// its own, and this click belongs to the press already down: it has no
/// `pointerdown`.
function liftOverBackdrop(chip: Locator): Promise<{ hit: string | null; inViewport: boolean; events: string[] }> {
  return chip.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const [x, y] = [r.left + r.width / 2, r.top + r.height / 2];
    const events: string[] = [];
    const note = (e: Event) => events.push(`${e.type} on ${(e.target as Element).getAttribute("data-testid")}`);
    const types = ["pointerup", "pointerleave", "click"];
    for (const type of types) document.addEventListener(type, note, true);
    // `chipPointer`'s pointer: the one that went down.
    const touch = { cancelable: true, composed: true, pointerId: 7, pointerType: "touch", isPrimary: true, clientX: x, clientY: y };
    try {
      el.dispatchEvent(new PointerEvent("pointerup", { ...touch, bubbles: true }));
      el.dispatchEvent(new PointerEvent("pointerleave", { ...touch, bubbles: false }));
      const at = document.elementFromPoint(x, y);
      const hit = at?.getAttribute("data-testid") ?? null;
      if (at && hit === "reaction-owners-backdrop") at.dispatchEvent(new PointerEvent("click", { ...touch, bubbles: true }));
      return { hit, inViewport: x >= 0 && y >= 0 && x <= window.innerWidth && y <= window.innerHeight, events };
    } finally {
      for (const type of types) document.removeEventListener(type, note, true);
    }
  });
}

/// A point near the history's top that a press outside the open list can
/// use: clear of the list, and on its backdrop.
async function backdropPointClearOf(page: Page, owners: Locator): Promise<{ x: number; y: number }> {
  const view = (await page.locator("#chat-scroll-container").boundingBox())!;
  const point = { x: view.x + view.width / 2, y: view.y + 40 };
  const list = (await owners.boundingBox())!;
  const onList = point.x >= list.x && point.x <= list.x + list.width && point.y >= list.y && point.y <= list.y + list.height;
  expect(onList, "premise: the outside point is clear of the list").toBe(false);
  expect(
    await page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid"), point),
    "premise: the outside press lands on the list's backdrop",
  ).toBe("reaction-owners-backdrop");
  return point;
}

type RecordedEvent = { type: string; on: string | null; pointerId: number | null };

/// Record the first touch pointer to go down on a reaction chip: that
/// `pointerdown`, every later event of the same pointer id, and every click
/// after it, wherever each lands (once the hold opens the list, its backdrop
/// covers the chip). From the document's capture phase, and listening only:
/// no handler of the app's is replaced and nothing is prevented. `stop`
/// removes the listeners and returns what was recorded; stopping twice is
/// harmless.
async function recordChipPointer(page: Page): Promise<{ read(): Promise<RecordedEvent[]>; stop(): Promise<RecordedEvent[]> }> {
  type Recorder = { events: RecordedEvent[]; stop(): void };
  await page.evaluate(() => {
    const events: RecordedEvent[] = [];
    let tracked: number | null = null;
    const types = ["pointerdown", "pointermove", "pointerup", "pointercancel", "click"];
    const note = (e: Event) => {
      const target = e.target instanceof Element ? e.target : null;
      const on = target?.closest('[data-testid="reaction-chip"]') ? "reaction-chip" : (target?.getAttribute("data-testid") ?? null);
      const pointerId = e instanceof PointerEvent ? e.pointerId : null;
      if (e.type === "click") {
        if (tracked !== null) events.push({ type: e.type, on, pointerId });
        return;
      }
      if (tracked === null) {
        if (e.type !== "pointerdown" || (e as PointerEvent).pointerType !== "touch" || on !== "reaction-chip") return;
        tracked = pointerId;
      }
      if (pointerId === tracked) events.push({ type: e.type, on, pointerId });
    };
    for (const type of types) document.addEventListener(type, note, true);
    (window as unknown as { __chipPointerRecord: Recorder }).__chipPointerRecord = {
      events,
      stop: () => types.forEach((type) => document.removeEventListener(type, note, true)),
    };
  });
  return {
    read: () => page.evaluate(() => (window as unknown as { __chipPointerRecord: Recorder }).__chipPointerRecord.events),
    stop: () =>
      page.evaluate(() => {
        const record = (window as unknown as { __chipPointerRecord: Recorder }).__chipPointerRecord;
        record.stop();
        return record.events;
      }),
  };
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
    const ownCount = await page.locator(OWN_CHIP).count();
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
    await expect(page.locator(OWN_CHIP), "nor remove the reaction").toHaveCount(ownCount);
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
      await expect(crab).toHaveAttribute("data-own-reaction", "false");
      await expect(crab.getByTestId("reaction-chip-count")).toHaveText("1");
      await owners.press("Escape");
      await expect(owners).toHaveCount(0);
      // Past CHIP_HOLD_MS (500).
      await page.waitForTimeout(700);
      await expect(owners, "a short tap is not a hold").toHaveCount(0);
      await expect(crab).toHaveAttribute("data-own-reaction", "false");
    }
  });

  test("a hold that ends without a lift does not swallow the next press outside", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    const chip = await signedChip(page);
    const owners = page.getByTestId("reaction-owners");
    for (const ending of ["pointercancel", "pointerleave"] as const) {
      // Down and held; the lift is never sent.
      await holdChip(page, chip);
      await expect(owners).toBeVisible();
      await chipPointer(chip, ending, ending === "pointercancel");
      // In the history, well above the chip and its list.
      const view = (await page.locator("#chat-scroll-container").boundingBox())!;
      await page.mouse.click(view.x + view.width / 2, view.y + 40);
      await expect(owners, `${ending}: one press outside closes the list`).toHaveCount(0);
    }
  });

  // `holdChip`'s own lift clicks the chip. A click there that the hold did
  // not take inspects instead, and over a list already open that shows
  // nothing. The app takes the lift's click on the list's backdrop too,
  // which covers the chip by then: here the click is sent there.
  test("the lift's click on the backdrop over the chip keeps the list, and the next press outside closes it", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    expect(await narrowLayout(page), "premise: 48rem or wider, where a held finger opens the list").toBe(false);
    const chip = await signedChip(page);
    const count = chip.getByTestId("reaction-chip-count");
    const owners = page.getByTestId("reaction-owners");
    const unchanged = async (why: string) => {
      await expect(chip, why).toHaveAttribute("data-own-reaction", "false");
      await expect(count, why).toHaveText("1");
    };

    // Down and held. The lift is this test's, not `holdChip`'s.
    await holdChip(page, chip);
    await expect(owners, "premise: the hold opened one list").toHaveCount(1);
    await expect(owners).toBeVisible();
    await expect(chip, "premise: for this chip, still there").toHaveAttribute("aria-expanded", "true");
    await unchanged("premise: someone else's reaction, one reactor");

    const lift = await liftOverBackdrop(chip);
    expect(lift.inViewport, "premise: the chip's centre is in the viewport").toBe(true);
    expect(lift.hit, `premise: the lift lands on the backdrop over the chip ${JSON.stringify(lift)}`).toBe(
      "reaction-owners-backdrop",
    );
    expect(lift.events, "the lift, the leave after it, then its click on the backdrop").toEqual([
      "pointerup on reaction-chip",
      "pointerleave on reaction-chip",
      "click on reaction-owners-backdrop",
    ]);
    await settle(page);
    await expect(owners, "the lift's click on the backdrop keeps the list open").toHaveCount(1);
    await expect(owners).toBeVisible();
    await unchanged("the lift's click does not react");

    // One press of its own, outside: the release was spent on the lift.
    await pressAt(page, await backdropPointClearOf(page, owners));
    await expect(owners, "the next press outside closes the list").toHaveCount(0);
    await settle(page);
    await unchanged("the press outside does not react");
    await expect(sheet(page), "nor open the message sheet").toBeHidden();
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
    const own = message.locator(OWN_CHIP);
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
    const ownCount = await page.locator(OWN_CHIP).count();
    await own.focus();
    await own.press("Enter");
    await expect(owners).toBeVisible();
    await expect(own, "the row's band keys on it").toHaveAttribute("aria-expanded", "true");
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await expect(owners).toBeFocused();
    await expect(page.locator(OWN_CHIP)).toHaveCount(ownCount);
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
    // That lift's click went where the browser sent it (the backdrop-lift
    // case in "Reaction owner list" sends one to the backdrop). Here too the
    // release is spent: one tap outside closes the list.
    const outside = await backdropPointClearOf(page, owners);
    await page.touchscreen.tap(outside.x, outside.y);
    await expect(owners, "one tap outside then closes it").toHaveCount(0);
    await expect(held, "and leaves the reaction").toHaveCount(1);
  });

  test("a held finger that drags away is cancelled, and one tap outside then closes the list", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    const owners = page.getByTestId("reaction-owners");
    const held = await addOwnReaction(page);
    const [x, y] = await centreOf(held);
    // Without these the test also passes for a drag the browser never
    // cancelled: its lift's click spends the release, and the tap closes.
    const pointer = await recordChipPointer(page);
    try {
      await withFingers(page, async (fingers) => {
        await fingers.press([1, x, y]);
        await expect(owners).toBeVisible();
        // The browser takes the drag for a scroll: pointercancel, and no click.
        await fingers.move({ 1: -60 });
        await expect
          .poll(async () => (await pointer.read()).map((e) => e.type), {
            message: "the drag cancelled the pointer that went down on the chip, before its lift",
          })
          .toContain("pointercancel");
        await fingers.lift();
      });
      await settle(page);
      // Stopped before the tap below, whose click is not this gesture's.
      const events = await pointer.stop();
      expect(events[0], "premise: the recorder saw that pointer go down on the chip").toMatchObject({
        type: "pointerdown",
        on: "reaction-chip",
      });
      expect(events.map((e) => e.type), "the gesture ended cancelled").toContain("pointercancel");
      expect(
        events.filter((e) => e.type === "click"),
        `the cancelled gesture sent no click ${JSON.stringify(events.map((e) => e.type))}`,
      ).toEqual([]);
      await expect(owners, "premise: the cancelled hold left the list open").toBeVisible();
      const view = (await page.locator("#chat-scroll-container").boundingBox())!;
      await page.touchscreen.tap(view.x + view.width / 2, view.y + 40);
      await expect(owners, "one tap outside closes it").toHaveCount(0);
    } finally {
      await pointer.stop().catch(() => {});
    }
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
    const party = chipFor(row, "🎉");
    const owners = page.getByTestId("reaction-owners");
    await expect(party.getByTestId("reaction-chip-count")).toHaveText("1");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));

    // Real touch exercises WebKit's touch-click-as-mouse behavior. Desktop
    // projects inspect from the keyboard because their mouse applies directly.
    if (hasTouch()) await party.tap();
    else await party.press("Enter");
    await expect(owners).toBeVisible();
    await expect(party).toHaveAttribute("data-own-reaction", "false");
    await expect(sheet(page), "a chip never opens the sheet").toBeHidden();
    await expect(owners.getByTestId("reaction-apply")).toHaveText("React with 🎉");
    await owners.getByTestId("reaction-apply").click();
    await expect(party.getByTestId("reaction-chip-count")).toHaveText("2");
    await expect(party).toHaveAttribute("data-own-reaction", "true");
    await removeFromList(page, party);
    await expect(party, "the shared chip stays").toHaveAttribute("data-own-reaction", "false");
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
    await expect(party).toHaveAttribute("data-own-reaction", "true");
    await expect(sheet(page)).toBeHidden();
  });

  test("a press outside the list only closes it, and the sheet stays closed", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await callRiverTest(page, "memberReactsToNewest", "🎉");
    const party = chipFor(row, "🎉");
    const owners = page.getByTestId("reaction-owners");
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    // Measured before the list opens: its backdrop then covers the row.
    const onRow = await plainPointOf(row);
    const hit = (p: { x: number; y: number }) =>
      page.evaluate(({ x, y }) => document.elementFromPoint(x, y)?.getAttribute("data-testid"), p);
    for (const [where, point] of [
      ["on its own message", onRow],
      ["far from it", { x: 195, y: 120 }],
    ] as const) {
      if (hasTouch()) await party.tap();
      else await party.press("Enter");
      await expect(owners).toBeVisible();
      expect(await hit(point), `premise, ${where}: the press lands on the backdrop`).toBe("reaction-owners-backdrop");
      await pressAt(page, point);
      await expect(owners, `${where}: the list closes`).toHaveCount(0);
      await settle(page);
      await expect(sheet(page), `${where}: the sheet stays closed`).toBeHidden();
    }
    // The same point opens the sheet once no list covers it.
    await pressAt(page, onRow);
    await expectSheetOpenFor(page, row);
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
  // 50 bytes: the default nickname cap.
  const LONG_NAME = "Bartholomew Featherstonehaugh-Cholmondeley of Bath";
  const PHONE = 390;
  const WIDE = 1280;

  type Placement = {
    chip: { left: number; width: number; centre: number };
    list: { left: number; right: number; width: number };
    /// The list's computed `max-width`, in CSS px.
    maxWidth: number;
    innerWidth: number;
    dialogId: string;
    /// Whether the list is the node `markList` marked.
    marked: boolean;
  };

  /// The chip and its open list as they are laid out now, read together.
  async function placement(chip: Locator): Promise<Placement> {
    const read = await chip.evaluate((el) => {
      const list = el.parentElement!.querySelector<HTMLElement>('[data-testid="reaction-owners"]');
      if (!list) return null;
      const c = el.getBoundingClientRect();
      const l = list.getBoundingClientRect();
      return {
        chip: { left: c.left, width: c.width, centre: c.left + c.width / 2 },
        list: { left: l.left, right: l.right, width: l.width },
        maxWidth: parseFloat(getComputedStyle(list).maxWidth),
        innerWidth: window.innerWidth,
        dialogId: list.id,
        marked: (list as unknown as { __sameList?: boolean }).__sameList === true,
      };
    });
    expect(read, "the chip's list is open").not.toBeNull();
    return read!;
  }

  /// Mark the open list's node, so a later read can tell it from a new one.
  function markList(owners: Locator): Promise<void> {
    return owners.evaluate((el) => {
      (el as unknown as { __sameList?: boolean }).__sameList = true;
    });
  }

  /// The app's policy (`horizontal_placement`): the roomier side of the
  /// window from the chip's centre, less 8px, never under 8rem. Restated
  /// here, so it shows the policy was applied to the layout as it is now,
  /// not that the policy is right (native tests have its arithmetic). Each
  /// case also requires the maximum to have changed across the resize.
  function policyMaxWidth(p: Placement): number {
    return Math.max(128, p.chip.centre - 8, p.innerWidth - p.chip.centre - 8);
  }

  /// The list's max width is the policy's, within 1px, for where its chip is
  /// now, in a window `width` wide. Polled: the placement lands a render
  /// after the resize. Returns the placement that matched.
  async function expectPlacedFor(chip: Locator, width: number, message: string): Promise<Placement> {
    let last: Placement | undefined;
    await expect
      .poll(
        async () => {
          last = await placement(chip);
          const policy = policyMaxWidth(last);
          const placed = Math.abs(last.maxWidth - policy) <= 1;
          return { window: last.innerWidth, maxWidth: placed ? "the policy's" : `${last.maxWidth}px, policy ${policy}px` };
        },
        { message, timeout: 5_000 },
      )
      .toEqual({ window: width, maxWidth: "the policy's" });
    return last!;
  }

  test("the list stays open and is placed again, on screen, when narrowing below 48rem", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    const chip = await signedChip(page, LONG_NAME);
    const owners = page.getByTestId("reaction-owners");
    await chip.press("Enter");
    await expect(owners.getByTestId("reaction-owner")).toHaveText(LONG_NAME);
    const { width: wideWidth, height } = page.viewportSize()!;
    const wide = await expectPlacedFor(chip, wideWidth, "premise: the wide list is placed for the wide window");
    // A list that could simply keep this placement on a phone proves nothing.
    expect(Math.abs(wide.list.left - wide.chip.left), "premise: the wide list hangs from its chip's left edge").toBeLessThanOrEqual(1);
    expect(wide.chip.left + wide.list.width, "premise: the wide list would overflow a phone from where its chip is").toBeGreaterThan(PHONE + 1);
    await markList(owners);

    await page.setViewportSize({ width: PHONE, height });
    await settle(page);
    await expect(owners, "the list remains available on a phone").toBeVisible();
    await expect(chip).toHaveAttribute("aria-expanded", "true");
    await expect(owners, "focus stays in the list").toBeFocused();
    const moved = await placement(chip);
    expect(moved.marked, "the same list, not one opened again").toBe(true);
    expect(moved.dialogId).toBe(wide.dialogId);
    // The chip moves when the sidebars go: from where it is now, too.
    expect(
      moved.chip.left + wide.list.width,
      "premise: the wide placement would overflow the phone from where the chip is now",
    ).toBeGreaterThan(PHONE + 1);
    const narrow = await expectPlacedFor(chip, PHONE, "narrowing: the list's max width is the phone window's placement");
    expect(Math.abs(narrow.maxWidth - wide.maxWidth), "narrowing: the max width changed from the wide placement's").toBeGreaterThan(1);
    await expectOnScreen(page, owners);
  });

  test("the list stays open and is placed again when widening past 48rem", async ({ page }) => {
    // The file's beforeEach widened a phone project; this case starts narrow.
    const { height } = page.viewportSize()!;
    await page.setViewportSize({ width: PHONE, height });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    const chip = await signedChip(page, LONG_NAME);
    const owners = page.getByTestId("reaction-owners");
    await chip.press("Enter");
    await expect(owners.getByTestId("reaction-owner")).toHaveText(LONG_NAME);
    await expect(owners).toBeFocused();
    const narrow = await expectPlacedFor(chip, PHONE, "premise: the phone list is placed for the phone window");
    await markList(owners);

    await page.setViewportSize({ width: WIDE, height });
    await settle(page);
    await expect(owners, "the list remains open on the wide window").toBeVisible();
    await expect(chip).toHaveAttribute("aria-expanded", "true");
    await expect(owners, "focus stays in the list").toBeFocused();
    const moved = await placement(chip);
    expect(moved.marked, "the same list, not one opened again").toBe(true);
    expect(moved.dialogId).toBe(narrow.dialogId);
    // On screen is not the oracle here: a phone-sized list fits any wider
    // window. Its permitted maximum is what the wide placement changes.
    const wide = await expectPlacedFor(chip, WIDE, "widening: the list's max width is the wide window's placement");
    expect(Math.abs(wide.maxWidth - narrow.maxWidth), "widening: the max width changed from the phone placement's").toBeGreaterThan(1);
    await expectOnScreen(page, owners);
  });
});
