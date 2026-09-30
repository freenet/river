import { test, expect, Page, Locator } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

// The reaction picker (ui/src/components/conversation/reaction_picker.rs): ONE
// popover for the whole conversation, opened by each message's "+". It lives in
// the top layer, so no scroll container clips it and the composer cannot cover
// it, and anchor positioning flips it to whichever side of the "+" has room.

const PLUS = '[data-testid="add-reaction-button"]';
const PICKER = '[data-testid="emoji-picker"]';
const CHIP = '[data-testid="reaction-chip"]';

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
});

/** Scroll the history so `plus`'s centre sits at viewport y = `y`. */
async function placeAt(plus: Locator, y: number) {
  await plus.evaluate((el, target) => {
    const scroller = document.getElementById("chat-scroll-container")!;
    const r = el.getBoundingClientRect();
    scroller.scrollBy(0, r.top + r.height / 2 - target);
  }, y);
}

/** A "+" from the middle of the history, so it can be scrolled anywhere. */
async function middlePlus(page: Page): Promise<Locator> {
  const all = page.locator(PLUS);
  await expect(all.first()).toBeAttached();
  return all.nth(Math.floor((await all.count()) / 2));
}

/** Open the picker from `plus` the way a person would. */
async function openFrom(page: Page, plus: Locator) {
  await plus.locator("xpath=ancestor::*[starts-with(@id,'msg-')][1]").hover();
  await plus.click();
  await expect(page.locator(PICKER)).toBeVisible();
}

/** Where the open picker sits, and whether anything paints over any part of it. */
const pickerLayout = (page: Page) =>
  page.evaluate((sel) => {
    const p = document.querySelector(sel)!;
    const r = p.getBoundingClientRect();
    // Edge midpoints 3px in, corners 8px in (inside the 12px rounding).
    const cx = (r.left + r.right) / 2;
    const cy = (r.top + r.bottom) / 2;
    const probes = [
      [cx, r.top + 3],
      [cx, r.bottom - 3],
      [r.left + 3, cy],
      [r.right - 3, cy],
      [r.left + 8, r.top + 8],
      [r.right - 8, r.top + 8],
      [r.left + 8, r.bottom - 8],
      [r.right - 8, r.bottom - 8],
    ];
    const covered = probes
      .map(([x, y]) => document.elementFromPoint(x, y))
      .filter((hit) => !hit || !p.contains(hit))
      .map((hit) =>
        hit ? `${hit.tagName.toLowerCase()}[${(hit as HTMLElement).dataset.testid ?? ""}]` : "nothing"
      );
    return { bottom: r.bottom, viewportHeight: window.innerHeight, covered };
  }, PICKER);

test("Escape closes it", async ({ page }) => {
  const plus = await middlePlus(page);
  await openFrom(page, plus);
  await expect(plus).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.press("Escape");
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(plus).toHaveAttribute("aria-expanded", "false");
});

test("clicking outside closes it", async ({ page }) => {
  const plus = await middlePlus(page);
  await openFrom(page, plus);
  // The empty gutter of the history, outside the picker and outside every row.
  await page.locator("#chat-scroll-container").click({ position: { x: 2, y: 2 } });
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(plus).toHaveAttribute("aria-expanded", "false");
});

test("picking an emoji reacts to that message and closes the picker", async ({ page }) => {
  // Pinned by id: once it holds a chip, a `:not(:has(chip))` locator would move to another row.
  const id = await page
    .locator(`[id^="msg-"]:not(:has(${CHIP}))`)
    .first()
    .evaluate((el) => el.id);
  const row = page.locator(`[id="${id}"]`);
  await row.scrollIntoViewIfNeeded();
  await openFrom(page, row.locator(PLUS));
  await page.locator(PICKER).locator("button").first().click();
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(row.locator(CHIP)).toHaveCount(1);
});

test.describe("on a short landscape screen", () => {
  // A phone held sideways: the history is ~250px tall between header and composer.
  test.use({ viewport: { width: 844, height: 390 } });

  test("nothing covers it", async ({ page }) => {
    // Top layer: the composer never paints over it.
    const plus = await middlePlus(page);
    await placeAt(plus, Math.round(390 * 0.55));
    await openFrom(page, plus);
    const l = await pickerLayout(page);
    expect(l.covered, "something paints over the picker").toEqual([]);
    expect(l.bottom).toBeLessThanOrEqual(l.viewportHeight + 1);
  });
});

test.describe("on a wide screen", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("opens beside the + that opened it", async ({ page }) => {
    const plus = await middlePlus(page);
    await placeAt(plus, Math.round(800 * 0.4));
    await plus.click();
    const picker = page.locator(PICKER);
    await expect(picker).toBeVisible();
    const b = (await plus.boundingBox())!;
    const p = (await picker.boundingBox())!;
    // Touches the "+" on the side it opened toward, below or above it, within the 0.25rem gap.
    const below = p.y >= b.y + b.height - 1 && p.y - (b.y + b.height) <= 8;
    const above = b.y >= p.y + p.height - 1 && b.y - (p.y + p.height) <= 8;
    expect(below || above, `picker y ${p.y}..${p.y + p.height} vs + y ${b.y}..${b.y + b.height}`).toBe(true);
    expect(p.x <= b.x + b.width && p.x + p.width >= b.x, `picker x ${p.x}..${p.x + p.width} vs + x ${b.x}..${b.x + b.width}`).toBe(true);
  });
});
