import { expect, Locator, Page, test } from "@playwright/test";
import { coarsePointer, selectRoom, settle, waitForApp } from "./example-room";

// Regression coverage for freenet/river#714.
//
// Reactor names were rendered only as the chip's native `title`. Desktop
// shows that on hover. A phone never does: there is no hover, and a tap does
// not surface `title` (an Android WebView does not surface it on long-press
// either). The chip of your own reaction removed it on click, so the one
// gesture a phone has destroyed the reaction instead of saying who left it.
//
// The fix keeps the desktop path (hover `title`, click removes your own
// reaction) and, where a finger can be the pointer, opens a list of names
// with an explicit Remove. Same pointer query as main.css:
// `(hover: none), (any-pointer: coarse)`.
//
// CI never caught this. No test read the names back on a coarse pointer, and
// a `title` attribute can be present while remaining unreachable. This test
// fails on mobile if the tap still does nothing or still removes the chip,
// and fails on desktop if the tap starts opening the list or stops removing
// your own reaction.

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
  if (!(await coarsePointer(page))) {
    await pinned.hover();
  }
  await pinned.getByTestId("add-reaction-button").click();
  await page.getByTestId("emoji-picker").getByRole("button").first().click();
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
  const messageId = await chip.evaluate((el) => el.closest('[id^="msg-"]')?.id ?? "");
  await pressChip(chip);
  const owners = page.getByTestId("reaction-owners");
  if (await coarsePointer(page)) {
    await expect(owners).toBeVisible();
    await owners.getByTestId("reaction-remove").click();
    await expect(owners).toHaveCount(0);
    await expect(chip).toHaveCount(0);
    // The chip that opened the list is gone, so focus goes to the nearest
    // control left on that message rather than nowhere.
    await expect(
      page.locator(`[id="${messageId}"]`).getByTestId("add-reaction-button")
    ).toBeFocused();
  } else {
    await expect(owners).toHaveCount(0);
  }
  await expect(chip).toHaveCount(0);
}

/// Scroll the history so `chip` is just out of view, without leaving the
/// rendered range.
async function scrollChipOutOfView(page: Page, chip: Locator): Promise<void> {
  await chip.evaluate((el) => {
    const scroller = document.getElementById("chat-scroll-container")!;
    const chipRect = el.getBoundingClientRect();
    const viewRect = scroller.getBoundingClientRect();
    const chipTop = chipRect.top - viewRect.top + scroller.scrollTop;
    const chipBottom = chipTop + chipRect.height;
    const max = scroller.scrollHeight - scroller.clientHeight;
    // Put the chip just below the view if there is room above it, else just
    // above the view.
    const below = chipTop - scroller.clientHeight - 24;
    scroller.scrollTop = below >= 0 ? below : Math.min(chipBottom + 24, max);
  });
  await settle(page);
  const inView = await chip.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const v = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return r.bottom > v.top && r.top < v.bottom;
  });
  expect(inView, "premise: the chip is scrolled out of view").toBe(false);
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

test.describe("Reaction owner list (#714)", () => {
  test("a coarse pointer can read who reacted; a fine pointer still removes on click", async ({
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

    if (coarse) {
      const ownCount = await page
        .locator('[data-testid="reaction-chip"][title*="click to remove"]')
        .count();
      const chipHeight = (await other.boundingBox())?.height ?? 0;

      await pressChip(other);
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

      // The backdrop covers the rest of the history, so this tap dismisses
      // and does not leave a second popover open.
      await page.mouse.click(8, 8);
      await expect(owners).toHaveCount(0);
      await expect(page.getByTestId("emoji-picker")).toHaveCount(0);
      await expect(page.getByTestId("message-action-menu")).toHaveCount(0);

      await pressChip(own);
      await expect(owners).toBeVisible();
      await expect(
        owners.getByTestId("reaction-owner").filter({ hasText: exactName("You") })
      ).toHaveCount(1);
      await expect(owners.getByTestId("reaction-remove")).toBeVisible();
      // Tap shows the names. It must not remove the reaction.
      await expect(
        page.locator('[data-testid="reaction-chip"][title*="click to remove"]')
      ).toHaveCount(ownCount);

      await page.mouse.click(8, 8);
      await expect(owners).toHaveCount(0);

      const added = await addOwnReaction(page);
      await removeOwnReaction(page, added);
    } else {
      const otherCount = await page
        .locator('[data-testid="reaction-chip"]:not([title*="click to remove"])')
        .count();

      await other.click();
      await expect(owners).toHaveCount(0);
      await expect(
        page.locator('[data-testid="reaction-chip"]:not([title*="click to remove"])')
      ).toHaveCount(otherCount);

      // A fine pointer does not open the list. The click removes this chip.
      // Wait for that message to drop it before adding another: the toggle
      // is deferred, and a room-wide count is not stable (fixture chips
      // that were never signed messages leave with the first real toggle).
      const ownMessageId = await own.evaluate((el) => el.closest('[id^="msg-"]')?.id ?? "");
      await own.click();
      await expect(owners).toHaveCount(0);
      await expect(
        page.locator(`[id="${ownMessageId}"]`).locator('[data-testid="reaction-chip"][title*="click to remove"]')
      ).toHaveCount(0);

      const added = await addOwnReaction(page);
      await removeOwnReaction(page, added);
    }
  });
});

test.describe("Reaction chip from the keyboard (#714)", () => {
  test("Enter and Space open the list, and closing it does not scroll the history", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);

    const owners = page.getByTestId("reaction-owners");

    // Your own reaction: a key press opens the list, with Remove in it, and
    // does not remove the reaction the way a mouse click does.
    const own = await chipByOwnReaction(page, true);
    const ownCount = await page
      .locator('[data-testid="reaction-chip"][title*="click to remove"]')
      .count();
    await own.focus();
    await own.press("Enter");
    await expect(owners).toBeVisible();
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await expect(owners).toBeFocused();
    await expect(
      page.locator('[data-testid="reaction-chip"][title*="click to remove"]')
    ).toHaveCount(ownCount);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);
    await expect(own).toBeFocused();

    // Space too. Then move the history so the chip is out of view while the
    // list is open: closing it puts focus back on the chip, and that must
    // not scroll the history back to it (only an arrival moves the view).
    const other = await chipByOwnReaction(page, false);
    await other.focus();
    await other.press(" ");
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
  });
});

test.describe("Reaction chip activated without a press (#714)", () => {
  test("a click with no fresh press of its own opens the list, never removes", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);

    const owners = page.getByTestId("reaction-owners");
    const ownSelector = '[data-testid="reaction-chip"][title*="click to remove"]';
    const own = await chipByOwnReaction(page, true);
    const ownCount = await page.locator(ownSelector).count();

    // What assistive technology does: a click event with no pointerdown.
    await own.evaluate((el) => (el as HTMLElement).click());
    await expect(owners).toBeVisible();
    await expect(owners.getByTestId("reaction-remove")).toBeVisible();
    await expect(page.locator(ownSelector)).toHaveCount(ownCount);
    await owners.press("Escape");
    await expect(owners).toHaveCount(0);

    // A mouse press on the chip dragged off it (no click there, and no
    // pointercancel), then later a click with no press of its own. The old
    // "mouse" press must not decide that click.
    const box = await own.boundingBox();
    expect(box).not.toBeNull();
    await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
    await page.mouse.down();
    await page.mouse.move(box!.x + box!.width / 2, box!.y - 120, { steps: 4 });
    await page.mouse.up();
    await page.waitForTimeout(1_800);
    await own.evaluate((el) => (el as HTMLElement).click());
    await expect(owners).toBeVisible();
    await expect(page.locator(ownSelector)).toHaveCount(ownCount);
  });
});

test.describe("Touchscreen with a mouse (#714)", { tag: "@chromium-only" }, () => {
  test.use({ hasTouch: true, viewport: { width: 1280, height: 800 } });

  test("a mouse click removes and a tap opens the list", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    expect(await coarsePointer(page)).toBe(true);

    const clicked = await addOwnReaction(page);
    await clicked.click();
    await expect(page.getByTestId("reaction-owners")).toHaveCount(0);
    await expect(clicked).toHaveCount(0);

    const tapped = await addOwnReaction(page);
    await tapped.tap();
    await expect(page.getByTestId("reaction-owners")).toBeVisible();
    await expect(tapped).toHaveCount(1);
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
