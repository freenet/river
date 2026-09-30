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
  await plus.click();
  await expect(page.locator(PICKER)).toBeVisible();
}

/** Something a tap opens a modal from, fully on screen and clear of `popover`: an author name, else the room title. */
async function modalOpenerClearOf(page: Page, popover: string): Promise<Locator> {
  await page.evaluate((sel) => {
    const p = document.querySelector(sel)!.getBoundingClientRect();
    const s = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    const clearOf = (r: DOMRect) => r.right < p.left || r.left > p.right || r.bottom < p.top || r.top > p.bottom;
    // Example history is random, so the view may hold no other author's group; the header title is the fallback.
    const names = [...document.querySelectorAll('[data-testid="message-group-header"] span[title^="Member ID"]')];
    const name = names.find((n) => {
      const r = n.getBoundingClientRect();
      return r.width > 0 && r.top >= s.top && r.bottom <= s.bottom && clearOf(r);
    });
    const title = document.querySelector('[data-testid="room-title-button"]');
    const pick = name ?? (title && clearOf(title.getBoundingClientRect()) ? title : null);
    document.querySelectorAll("[data-test-opener]").forEach((e) => e.removeAttribute("data-test-opener"));
    pick?.setAttribute("data-test-opener", "");
  }, popover);
  return page.locator("[data-test-opener]");
}

/** The member-info and room-details modals, which the openers above open. */
const MODALS = '[data-testid="member-info-modal"], [data-testid="edit-room-modal"]';

/** Whatever paints over any part of the open picker. */
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
    return { covered };
  }, PICKER);

test("Escape or a click outside closes it", async ({ page }) => {
  const plus = await middlePlus(page);
  await openFrom(page, plus);
  await expect(plus).toHaveAttribute("aria-expanded", "true");
  await page.keyboard.press("Escape");
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(plus).toHaveAttribute("aria-expanded", "false");

  await openFrom(page, plus);
  await expect(plus).toHaveAttribute("aria-expanded", "true");
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
  await openFrom(page, row.locator(PLUS));
  await page.locator(PICKER).locator("button").first().click();
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(row.locator(CHIP)).toHaveCount(1);
});

test("switching rooms closes it", async ({ page }) => {
  await openFrom(page, await middlePlus(page));
  // By keyboard: a click outside would light-dismiss the picker before the room changes.
  const room = page.getByTestId("room-list").getByRole("button", { name: "Team Chat Room" });
  if (!(await room.isVisible())) {
    await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).focus();
    await page.keyboard.press("Enter");
  }
  await room.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("heading", { name: "Team Chat Room" })).toBeVisible();
  await expect(page.locator(PICKER)).toBeHidden();
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
  });

  test("opens beside the + that opened it, flipped to stay on screen", async ({ page }) => {
    // Low on the screen there is less room below the "+" than the picker is tall, so it has to flip above.
    const plus = await middlePlus(page);
    await placeAt(plus, 290);
    await openFrom(page, plus);
    const b = (await plus.boundingBox())!;
    const p = (await page.locator(PICKER).boundingBox())!;
    // Touches the "+" on the side it opened toward, below or above it, within the 0.25rem gap.
    const below = p.y >= b.y + b.height - 1 && p.y - (b.y + b.height) <= 8;
    const above = b.y >= p.y + p.height - 1 && b.y - (p.y + p.height) <= 8;
    expect(below || above, `picker y ${p.y}..${p.y + p.height} vs + y ${b.y}..${b.y + b.height}`).toBe(true);
    expect(p.x <= b.x + b.width && p.x + p.width >= b.x, `picker x ${p.x}..${p.x + p.width} vs + x ${b.x}..${b.x + b.width}`).toBe(true);
    expect(p.y).toBeGreaterThanOrEqual(0);
    expect(p.y + p.height).toBeLessThanOrEqual(390 + 1);
  });
});

test("its panels and buttons are hidden where the Popover API is missing", async ({ page }) => {
  // No supporting engine can run the guard, so read the rule through the CSSOM, which keeps it either way.
  const hidden = await page.evaluate(() => {
    const rules = [...document.styleSheets].flatMap((s) => {
      try {
        return [...s.cssRules];
      } catch {
        return [];
      }
    });
    const guard = rules.find(
      (r) => r instanceof CSSSupportsRule && /not\s+selector\(:popover-open\)/.test(r.conditionText)
    ) as CSSSupportsRule | undefined;
    return guard ? [...guard.cssRules].map((r) => (r as CSSStyleRule).selectorText).join(",") : "";
  });
  for (const sel of ["#reaction-picker", "#message-action-menu", ".add-reaction-btn", ".touch-actions"]) {
    expect(hidden).toContain(sel);
  }
});

test("the tap that closes it does nothing else", async ({ page }) => {
  await openFrom(page, await middlePlus(page));
  await (await modalOpenerClearOf(page, PICKER)).click();
  await expect(page.locator(PICKER)).toBeHidden();
  // Light dismiss closed the picker; the same tap must not also open a modal.
  await page.waitForTimeout(300);
  await expect(page.locator(MODALS)).toHaveCount(0);
});

test("moving focus out closes it, so a modal never opens under it", async ({ page }) => {
  const plus = await middlePlus(page);
  await plus.focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(PICKER)).toBeVisible();
  const r = (await page.locator(PICKER).boundingBox())!;
  // Tab's route: focus lands on a modal's opener outside the picker, then Enter opens the modal.
  await page.getByTestId("room-info-button").focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(page.getByTestId("edit-room-modal")).toBeVisible();
  const covered = await page.evaluate(
    ([x, y]) => !!document.elementFromPoint(x, y)?.closest(".fixed")?.querySelector('[data-testid="edit-room-modal"]'),
    [r.x + r.width / 2, r.y + r.height / 2]
  );
  expect(covered, "the picker's place is under the modal, not over it").toBe(true);
});

test("deleting the message closes its picker", async ({ page }) => {
  const id = await page.locator('[id^="msg-"]:has(.bg-accent)').last().evaluate((el) => el.id);
  // By keyboard: nothing light-dismisses it on the way.
  await page.locator(`[id="${id}"]`).locator(PLUS).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(PICKER)).toBeVisible();
  // Deleted on another device while the picker is open.
  await page.evaluate(() => (window as any).__riverTest.deleteLastOwnMessage(0));
  await expect(page.locator(`[id="${id}"]`)).toHaveCount(0);
  await expect.poll(() => page.locator(PICKER).evaluate((el) => el.matches(":popover-open"))).toBe(false);
});

test("opening it by keyboard focuses the first emoji; Esc returns focus to the +", async ({ page }) => {
  const plus = await middlePlus(page);
  await plus.focus();
  await page.keyboard.press("Enter");
  await expect(page.getByRole("dialog", { name: /reaction/i })).toBeVisible();
  await expect.poll(() => page.evaluate(() => !!document.activeElement?.closest("#reaction-picker"))).toBe(true);
  await page.keyboard.press("Escape");
  await expect(page.locator(PICKER)).toBeHidden();
  await expect(plus).toBeFocused();
});

test("on page load neither popover holds focus", async ({ page }) => {
  // `autofocus` inside a closed popover must not take focus when the page loads.
  expect(
    await page.evaluate(() => !!document.activeElement?.closest("#reaction-picker, #message-action-menu"))
  ).toBe(false);
});
