import { expect, Locator, Page, test } from "@playwright/test";
import { selectRoom, waitForApp } from "./example-room";

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

async function coarsePointer(page: Page): Promise<boolean> {
  return page.evaluate(() =>
    window.matchMedia("(hover: none), (any-pointer: coarse)").matches
  );
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

// Playwright's click() is a mouse pointer even on a touch project.
// The app trusts pointerType, so a finger on mobile has to be tap().
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
  const owners = page.getByTestId("reaction-owners");
  if (await coarsePointer(page)) {
    await expect(owners).toBeVisible();
    await owners.getByTestId("reaction-remove").click();
    await expect(owners).toHaveCount(0);
  } else {
    await expect(owners).toHaveCount(0);
  }
  await expect(chip).toHaveCount(0);
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
