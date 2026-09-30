import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectListedRoom, resolveColor, computedColor } from "./example-room";

// A reply's quote strip jumps to the quoted row and lights the whole row for 2s.
// "Public Discussion Room": self is an observer, so every group has an author header,
// and the first strip quotes the history's first message, the first row of its group.
test.use({ viewport: { width: 1280, height: 800 } });

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await selectListedRoom(page, "Public Discussion Room");
});

const strip = (page: Page) => page.getByTestId("reply-strip").first();

// The live reply-jump highlight on row `id` (its currentTime in ms), or null.
const highlight = (page: Page, id: string) =>
  page.evaluate((id) => {
    const a = document
      .getElementById(id)
      ?.getAnimations()
      .find((a) => a.id === "reply-highlight" && a.playState === "running");
    return a ? Number(a.currentTime) : null;
  }, id);

test("activating the strip again restarts the highlight", async ({ page }) => {
  const target = (await strip(page).getAttribute("data-reply-target"))!;
  await strip(page).click();
  await expect.poll(() => highlight(page, target)).not.toBeNull();

  // Seek the live highlight to 1500ms and activate the strip again in the same task:
  // no wall-clock wait, so a second click that does nothing leaves it at 1500ms and running.
  await page.evaluate((id) => {
    const live = document
      .getElementById(id)!
      .getAnimations()
      .find((a) => a.id === "reply-highlight")!;
    live.currentTime = 1500;
    (document.querySelector(`[data-reply-target="${id}"]`) as HTMLElement).click();
  }, target);

  await expect
    .poll(async () => {
      const t = await highlight(page, target);
      return t !== null && t < 1000;
    })
    .toBe(true);
});

test("the highlight fills the quoted row, author name included", async ({ page }) => {
  const target = (await strip(page).getAttribute("data-reply-target"))!;
  await strip(page).click();

  // Bubble grey on the row itself (not on an inset box), compared as resolved colours.
  const surface = await resolveColor(page, "var(--color-surface)");
  await expect.poll(() => computedColor(page.locator(`[id="${target}"]`), "backgroundColor")).toEqual(surface);
});

// No @chromium-only: whether an animation ending starts a CSS transition is engine behaviour.
test("the highlight switches off with no fade, even under the pointer", async ({ page }) => {
  const target = (await strip(page).getAttribute("data-reply-target"))!;
  await strip(page).click();
  await expect.poll(() => highlight(page, target)).not.toBeNull();

  // The row's hover band fades in over 150ms; let that settle so only an ending highlight can start one.
  await page.locator(`[id="${target}"]`).hover();
  await expect
    .poll(() =>
      page.evaluate(
        (id) => document.getElementById(id)!.getAnimations().filter((a) => a instanceof CSSTransition).length,
        target
      )
    )
    .toBe(0);

  const transitionsAfterEnd = await page.evaluate(async (id) => {
    const row = document.getElementById(id)!;
    row.getAnimations().find((a) => a.id === "reply-highlight")!.finish();
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    return row
      .getAnimations()
      .filter((a) => a instanceof CSSTransition && a.transitionProperty === "background-color").length;
  }, target);
  expect(transitionsAfterEnd).toBe(0);
});

test("pressing Enter or Space on the focused reply strip scrolls to the original", async ({ page }) => {
  const target = (await strip(page).getAttribute("data-reply-target"))!;

  // A native button activates on both keys with no key handler. `.focus()`, not Tab:
  // WebKit leaves native buttons out of the plain Tab order.
  await strip(page).focus();
  await page.keyboard.press("Enter");
  await expect.poll(() => highlight(page, target)).not.toBeNull();

  await page.evaluate(
    (id) =>
      document
        .getElementById(id)
        ?.getAnimations()
        .filter((a) => a.id === "reply-highlight")
        .forEach((a) => a.finish()),
    target
  );
  await expect.poll(() => highlight(page, target)).toBeNull();

  await page.keyboard.press("Space");
  await expect.poll(() => highlight(page, target)).not.toBeNull();
});
