import { test, expect, Locator, Page } from "@playwright/test";
import {
  centreOf,
  chips,
  historyPoint,
  openMessageActions,
  openRoomWithTarget,
  openSheetFromReact,
  palette,
  panelAtRest,
  panelBottomGap,
  panelOffset,
  pressAt,
  settle,
  SHEET,
  sheet,
  withFingers,
} from "./example-room";
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

test("the row shows no scroll bar and takes only sideways pans", async ({ page }) => {
  await openSheet(page);
  const g = await page.evaluate(() => {
    const row = document.querySelector("#mobile-message-sheet .mobile-reaction-palette") as HTMLElement;
    const css = (sel: string) => getComputedStyle(document.querySelector(sel)!);
    return {
      gutter: row.offsetHeight - row.clientHeight,
      row: getComputedStyle(row).touchAction,
      backdrop: css("#mobile-message-sheet .mobile-sheet-backdrop").touchAction,
      panel: css("#mobile-message-sheet .mobile-sheet-panel").touchAction,
    };
  });
  expect(g.gutter, "no scroll bar takes room under the row").toBe(0);
  expect(g.row, "the row pans only sideways").toBe("pan-x");
  expect(g.backdrop, "the backdrop leaves every move to the drag").toBe("none");
  expect(g.panel).toBe("none");
});

test("with reduced motion it opens and closes without sliding", { tag: "@chromium-only" }, async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const row = await openSheet(page);
  expect(await page.locator(`#${SHEET} .mobile-sheet-panel`).evaluate((p) => p.getAnimations().length), "no opening slide").toBe(0);
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

  test("a slow drag down moves the panel with the finger, and springs back when let go short", async ({ page }) => {
    const row = await openSheet(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, 30] }, 20);
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

  test("dragged past the threshold it slides away and closes, reacting with nothing", async ({ page }) => {
    const row = await openSheet(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, (fingers) => fingers.drag(x, y, 120));
    await expect(sheet(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();
  });

  test("a drag down that starts over the history drags it and closes it, and scrolls nothing", async ({ page }) => {
    await openSheet(page);
    const history = page.locator("#chat-scroll-container");
    const before = await history.evaluate((el) => el.scrollTop);
    const h = await historyPoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, h.x, h.y]);
      await fingers.move({ 1: [0, 60] }, 10);
      expect(await panelOffset(page), "the panel moves with a finger above it").toBeGreaterThan(50);
      await fingers.move({ 1: [0, 90] }, 10);
      await fingers.lift();
    });
    await expect(sheet(page)).toBeHidden();
    expect(await history.evaluate((el) => el.scrollTop), "the history did not move").toBe(before);
  });

  test("a drag down that starts on an emoji closes it without reacting", async ({ page }) => {
    const row = await openSheet(page);
    const signs = await callRiverTest(page, "messageSignsRecorded");
    const [x, y] = await centreOf(palette(page).getByRole("button").nth(2));
    await withFingers(page, (fingers) => fingers.drag(x, y, 120));
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    await expect(chips(row)).toHaveCount(0);
    expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
  });

  test("a drag down that starts on Reply closes it without replying", async ({ page }) => {
    // Opened by a tap on the row: the same sheet, the same drag.
    await openMessageActions(page, await openRoomWithTarget(page));
    const [x, y] = await centreOf(page.getByTestId("mobile-message-action-reply"));
    await withFingers(page, (fingers) => fingers.drag(x, y, 150));
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    await expect(page.getByTestId("message-composer"), "no reply began").not.toContainText("Replying to");
  });

  test("dragging up never lifts it above its rest position", async ({ page }) => {
    await openSheet(page);
    const [x, y] = await handlePoint(page);
    expect(await panelBottomGap(page), "premise: at rest on the bottom edge").toBeLessThan(1);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, -100] }, 10);
      expect(await panelOffset(page), "no upward offset").toBe(0);
      expect(await panelBottomGap(page), "no gap under it").toBeLessThan(1);
      // Back down past the start: it follows from where the finger went down.
      await fingers.move({ 1: [0, 130] }, 10);
      expect(await panelOffset(page)).toBeGreaterThan(20);
      await fingers.lift();
    });
    await panelAtRest(page);
    expect(await panelOffset(page)).toBe(0);
    await expect(sheet(page)).toBeVisible();
  });

  test("a second finger cancels the drag", async ({ page }) => {
    await openSheet(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, 30] }, 10);
      expect(await panelOffset(page), "premise: dragging").toBeGreaterThan(20);
      await fingers.press([2, x + 60, y - 100]);
      await panelAtRest(page);
      expect(await panelOffset(page), "a pinch is no drag: back at rest").toBe(0);
      await fingers.move({ 1: [0, 100], 2: [0, -100] }, 10);
      await fingers.lift();
    });
    await settle(page);
    await expect(sheet(page), "still open").toBeVisible();
  });

  test("a sideways swipe on the row scrolls it, not the panel or the history, and is not a tap", async ({ page }) => {
    const row = await openSheet(page);
    const history = page.locator("#chat-scroll-container");
    const before = await history.evaluate((el) => el.scrollTop);
    const box = (await palette(page).boundingBox())!;
    const [x, y] = [Math.round(box.x + box.width - 30), Math.round(box.y + box.height / 2)];
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [-100, 6] }, 10);
      expect(await panelOffset(page), "mid-swipe: the panel stays").toBe(0);
      await fingers.move({ 1: [-100, 0] }, 10);
      await fingers.lift();
    });
    await settle(page);
    expect(await palette(page).evaluate((el) => el.scrollLeft), "the row scrolled").toBeGreaterThan(50);
    expect(await panelOffset(page)).toBe(0);
    expect(await history.evaluate((el) => el.scrollTop), "the history did not move").toBe(before);
    await expect(sheet(page), "a swipe is not a tap: still open").toBeVisible();
    await expect(chips(row)).toHaveCount(0);
  });
});
