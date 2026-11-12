import { test, expect, Locator, Page } from "@playwright/test";
import {
  BACKDROP,
  centreOf,
  chips,
  choices,
  historyPoint,
  openRoomWithTarget,
  openSheetFromReact,
  palette,
  PANEL,
  panelAtRest,
  panelBottomGap,
  panelOffset,
  pressAt,
  settle,
  SHEET,
  sheet,
  withFingers,
} from "./example-room";
import { scrollMetrics } from "./history-geometry";
import { callRiverTest } from "./river-test";

// The mobile message sheet's gestures (mobile_sheet_drag.rs;
// mobile-message-sheet.spec.ts has the rest of it). Its emoji row scrolls
// sideways with no scroll bar, and a downward drag that starts anywhere on
// the screen drags the panel and dismisses it once far or fast enough (else
// it springs back). Real touch input needs Chromium's input protocol (CDP),
// so the finger tests run on chromium and mobile-chrome; the mouse tests run
// on the desktop engines.

/// A received message with no reactions, its sheet open and at rest.
async function openSheet(page: Page): Promise<Locator> {
  const row = await openRoomWithTarget(page);
  await openSheetFromReact(page, row);
  return row;
}

/// The middle of the panel's handle.
function handlePoint(page: Page): Promise<[number, number]> {
  return centreOf(page.locator(`#${SHEET} .mobile-sheet-handle`));
}

test.use({ viewport: { width: 390, height: 760 } });

test("the row shows no scroll bar; one finger pans only the row, and a pinch zooms everywhere", async ({ page }) => {
  await openSheet(page);
  const touchAction = (target: Locator) => target.evaluate((el) => getComputedStyle(el).touchAction);
  expect(await palette(page).evaluate((row) => row.offsetHeight - row.clientHeight), "no scroll bar takes room under the row").toBe(0);
  expect(await touchAction(palette(page)), "the row pans only sideways").toBe("pan-x pinch-zoom");
  expect(await touchAction(page.getByTestId(BACKDROP)), "the backdrop takes no pan: the drag has every one-finger move").toBe("pinch-zoom");
  expect(await touchAction(page.locator(PANEL))).toBe("pinch-zoom");
  // Every element from a choice up to the root allows the zoom.
  const blockers = await choices(page)
    .first()
    .evaluate((el) => {
      const out: string[] = [];
      for (let e: Element | null = el; e; e = e.parentElement) {
        const t = getComputedStyle(e).touchAction;
        if (!/auto|manipulation|pinch-zoom/.test(t)) out.push(`${e.tagName}.${e.className}: ${t}`);
      }
      return out;
    });
  expect(blockers, "nothing between a choice and the root forbids a pinch").toEqual([]);
});

test("with reduced motion it opens and closes without sliding", { tag: "@chromium-only" }, async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const row = await openSheet(page);
  expect(await page.locator(PANEL).evaluate((p) => p.getAnimations().length), "no opening slide").toBe(0);
  await sheet(page).evaluate((d) => {
    const w = window as unknown as { __left: boolean };
    w.__left = false;
    new MutationObserver(() => (w.__left ||= d.classList.contains("is-leaving"))).observe(d, { attributes: true });
  });
  await pressAt(page, await historyPoint(page));
  await expect(sheet(page)).toBeHidden();
  expect(await page.evaluate(() => (window as unknown as { __left: boolean }).__left), "it never slid away").toBe(false);
  await expect(chips(row)).toHaveCount(0);
});

test.describe("A mouse on the message sheet", () => {
  test.skip(({ hasTouch }) => hasTouch, "a mouse drag, on the desktop engines");

  test("a mouse drag down closes it; a short or sideways one on the backdrop does not", async ({ page }) => {
    const row = await openSheet(page);
    const [x, y] = await handlePoint(page);
    await page.mouse.move(x, y);
    await page.mouse.down();
    // Inside the viewport: a mouse let go outside the window sends no
    // pointerup, and the drag springs back.
    await page.mouse.move(x, y + 60, { steps: 6 });
    await page.mouse.up();
    await expect(sheet(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);

    await openSheetFromReact(page, row);
    const h = await historyPoint(page);
    await page.mouse.move(h.x, h.y);
    await page.mouse.down();
    await page.mouse.move(h.x, h.y + 20, { steps: 5 });
    // Held still before letting go: a slow drag, not a flick (the engines
    // send these steps a few ms apart).
    await page.waitForTimeout(150);
    await page.mouse.up();
    await settle(page);
    await expect(sheet(page), "dragged short: springs back").toBeVisible();
    await panelAtRest(page);
    expect(await panelOffset(page)).toBe(0);

    // A sideways drag on the backdrop starts no drag, and the click that
    // ends it is no tap.
    await page.mouse.move(h.x, h.y);
    await page.mouse.down();
    await page.mouse.move(h.x + 60, h.y, { steps: 5 });
    await page.mouse.up();
    await settle(page);
    await expect(sheet(page), "a sideways drag is no tap").toBeVisible();
    expect(await panelOffset(page)).toBe(0);
  });
});

test.describe("The message sheet's opening slide", () => {
  // `showModal()` focuses the first emoji while the slide still holds the
  // panel below the dialog. A dialog that can scroll is scrolled to reveal
  // it: WebKit kept that scroll for the whole slide, so the panel rose its
  // own height above the bottom edge and dropped back when the slide ended;
  // Chromium clamped it each frame, so the panel never slid at all.
  test("never scrolls the dialog or lifts the panel off the bottom edge", async ({ page }) => {
    const row = await openRoomWithTarget(page);
    await page.evaluate((id) => {
      const frames: { scrollTop: number; gap: number }[] = [];
      (window as unknown as { __sheetFrames: typeof frames }).__sheetFrames = frames;
      const sample = () => {
        const dialog = document.getElementById(id) as HTMLDialogElement | null;
        const panel = dialog?.querySelector(".mobile-sheet-panel");
        if (dialog?.open && panel) {
          frames.push({ scrollTop: dialog.scrollTop, gap: window.innerHeight - panel.getBoundingClientRect().bottom });
        }
        requestAnimationFrame(sample);
      };
      requestAnimationFrame(sample);
    }, SHEET);
    await openSheetFromReact(page, row);
    await settle(page);
    const frames = await page.evaluate(() => (window as unknown as { __sheetFrames: { scrollTop: number; gap: number }[] }).__sheetFrames);
    expect(frames.length, "premise: frames were sampled while it was open").toBeGreaterThan(0);
    expect(Math.max(...frames.map((f) => f.scrollTop)), "the dialog never scrolls").toBe(0);
    expect(Math.max(...frames.map((f) => f.gap)), "never above the bottom edge").toBeLessThan(1);
  });
});

test.describe("A finger on the message sheet", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "real touch input needs Chromium's input protocol");

  test("dragged past the threshold from Reply it slides away and closes, reacting and replying with nothing", async ({ page }) => {
    const row = await openSheet(page);
    const [x, y] = await centreOf(page.getByTestId("mobile-message-action-reply"));
    await withFingers(page, (fingers) => fingers.drag(x, y, 150));
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    await expect(chips(row)).toHaveCount(0);
    await expect(page.getByTestId("message-composer"), "no reply began").not.toContainText("Replying to");
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();
  });

  test("a drag down that starts over the history drags it and closes it, and scrolls nothing", async ({ page }) => {
    await openSheet(page);
    const before = (await scrollMetrics(page)).top;
    const h = await historyPoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, h.x, h.y]);
      await fingers.move({ 1: [0, 60] });
      expect(await panelOffset(page), "the panel moves with a finger above it").toBeGreaterThan(50);
      await fingers.move({ 1: [0, 90] });
      await fingers.lift();
    });
    await expect(sheet(page)).toBeHidden();
    expect((await scrollMetrics(page)).top, "the history did not move").toBe(before);
  });

  test("a drag down that starts on an emoji closes it without reacting", async ({ page }) => {
    const row = await openSheet(page);
    const signs = await callRiverTest(page, "messageSignsRecorded");
    const [x, y] = await centreOf(choices(page).nth(2));
    await withFingers(page, (fingers) => fingers.drag(x, y, 120));
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    await expect(chips(row)).toHaveCount(0);
    expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
  });

  test("dragging up never lifts it above its rest position; back down it follows the finger, and springs back let go short", async ({ page }) => {
    const row = await openSheet(page);
    const [x, y] = await handlePoint(page);
    expect(await panelBottomGap(page), "premise: at rest on the bottom edge").toBeLessThan(1);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, -100] });
      expect(await panelOffset(page), "no upward offset").toBe(0);
      expect(await panelBottomGap(page), "no gap under it").toBeLessThan(1);
      // Slowly back down, 30px past the start: it follows from where the
      // finger went down.
      await fingers.move({ 1: [0, 130] }, 40);
      const held = await panelOffset(page);
      expect(held, "it follows the finger").toBeGreaterThan(27);
      expect(held).toBeLessThan(33);
      await fingers.lift();
    });
    await panelAtRest(page);
    expect(await panelOffset(page), "sprung back").toBe(0);
    await expect(sheet(page)).toBeVisible();
    await expect(chips(row)).toHaveCount(0);
  });

  test("a second finger cancels the drag", async ({ page }) => {
    await openSheet(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, 30] });
      expect(await panelOffset(page), "premise: dragging").toBeGreaterThan(20);
      await fingers.press([2, x + 60, y - 100]);
      await panelAtRest(page);
      expect(await panelOffset(page), "a pinch is no drag: back at rest").toBe(0);
      await fingers.move({ 1: [0, 100], 2: [0, -100] });
      await fingers.lift();
    });
    await settle(page);
    await expect(sheet(page), "still open").toBeVisible();
  });

  for (const where of ["the backdrop", "the emoji row", "the actions"] as const) {
    test(`a pinch over ${where} zooms the page and leaves the sheet open`, async ({ page }) => {
      await openSheet(page);
      const h = await historyPoint(page);
      const [x, y] =
        where === "the backdrop"
          ? [h.x + 120, h.y + 100]
          : await centreOf(where === "the emoji row" ? palette(page) : page.getByTestId("mobile-message-action-reply"));
      expect(await page.evaluate(() => window.visualViewport!.scale), "premise: not zoomed").toBe(1);
      await withFingers(page, async (fingers) => {
        await fingers.press([1, x - 20, y], [2, x + 20, y]);
        await fingers.move({ 1: [-80, 0], 2: [80, 0] });
        await fingers.lift();
      });
      expect(await page.evaluate(() => window.visualViewport!.scale), "the pinch zoomed").toBeGreaterThan(1.5);
      await expect(sheet(page), "still open").toBeVisible();
    });
  }

  test("a sideways swipe on the row scrolls it, not the panel or the history, and is not a tap", async ({ page }) => {
    const row = await openSheet(page);
    const before = (await scrollMetrics(page)).top;
    const box = (await palette(page).boundingBox())!;
    const [x, y] = [Math.round(box.x + box.width - 30), Math.round(box.y + box.height / 2)];
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [-100, 6] });
      expect(await panelOffset(page), "mid-swipe: the panel stays").toBe(0);
      await fingers.move({ 1: [-100, 0] });
      await fingers.lift();
    });
    await settle(page);
    expect(await palette(page).evaluate((el) => el.scrollLeft), "the row scrolled").toBeGreaterThan(50);
    expect(await panelOffset(page)).toBe(0);
    expect((await scrollMetrics(page)).top, "the history did not move").toBe(before);
    await expect(sheet(page), "a swipe is not a tap: still open").toBeVisible();
    await expect(chips(row)).toHaveCount(0);
  });
});
