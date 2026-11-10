import { test, expect, Locator, Page } from "@playwright/test";
import {
  committedRow,
  historyPoint,
  openMessageActions,
  openReactionPopup,
  openRoomWithComposer,
  panelAtRest,
  panelBottomGap,
  panelOffset,
  pressAt,
  settle,
  touchmovePrevented,
  withFingers,
} from "./example-room";
import { deliver } from "./history-geometry";
import { callRiverTest } from "./river-test";

// The mobile surfaces' gestures (mobile_sheet_drag.rs), mostly through the
// reaction popup; the actions sheet shares them (its own block at the end).
// The popup's emoji row
// scrolls sideways with no scroll bar, a downward drag that starts anywhere
// on the screen drags the panel and dismisses it once far or fast enough
// (else it springs back), and a tap outside slides it away. Real touch
// input needs Chromium's input protocol (CDP), so the finger tests run on
// chromium and mobile-chrome; the mouse tests run on the desktop engines.

const POPUP = "mobile-message-sheet";

function popup(page: Page): Locator {
  return page.getByTestId(POPUP);
}

function palette(page: Page): Locator {
  return page.getByTestId("mobile-reaction-palette");
}

function chips(row: Locator): Locator {
  return row.getByTestId("reaction-chip");
}

/// A received message with no reactions, its popup open and at rest.
async function openPopup(page: Page): Promise<Locator> {
  await openRoomWithComposer(page);
  await deliver(page, "react target");
  const row = committedRow(page, "react target");
  await expect(row).toHaveCount(1);
  await openReactionPopup(page, row);
  await panelAtRest(page, POPUP);
  return row;
}

/// The middle of the panel's handle.
async function handlePoint(page: Page): Promise<[number, number]> {
  const box = (await page.locator(`#${POPUP} .mobile-sheet-handle`).boundingBox())!;
  return [Math.round(box.x + box.width / 2), Math.round(box.y + box.height / 2)];
}

/// The middle of the `n`th emoji.
async function choicePoint(page: Page, n: number): Promise<[number, number]> {
  const box = (await palette(page).getByRole("button").nth(n).boundingBox())!;
  return [Math.round(box.x + box.width / 2), Math.round(box.y + box.height / 2)];
}

test.use({ viewport: { width: 390, height: 760 } });

test("the row shows no scroll bar, does not snap, and takes only sideways pans", async ({ page }) => {
  await openPopup(page);
  const g = await page.evaluate(() => {
    const row = document.querySelector("#mobile-message-sheet .mobile-reaction-palette") as HTMLElement;
    const css = (sel: string) => getComputedStyle(document.querySelector(sel)!);
    return {
      gutter: row.offsetHeight - row.clientHeight,
      snap: getComputedStyle(row).scrollSnapType,
      row: getComputedStyle(row).touchAction,
      backdrop: css("#mobile-message-sheet .mobile-sheet-backdrop").touchAction,
      panel: css("#mobile-message-sheet .mobile-sheet-panel").touchAction,
    };
  });
  expect(g.gutter, "no scroll bar takes room under the row").toBe(0);
  expect(g.snap, "free scrolling, no snapping").toBe("none");
  expect(g.row, "the row pans only sideways").toBe("pan-x");
  expect(g.backdrop, "the backdrop leaves every move to the drag").toBe("none");
  expect(g.panel).toBe("none");
});

test("a tap outside slides it away and closes it; reopened, it is at rest", async ({ page }) => {
  const row = await openPopup(page);
  await pressAt(page, await historyPoint(page));
  await expect(popup(page)).toBeHidden();
  await expect(chips(row)).toHaveCount(0);
  await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();
  await openReactionPopup(page, row);
  await panelAtRest(page, POPUP);
  expect(await panelOffset(page, POPUP)).toBe(0);
  expect(await popup(page).evaluate((d) => [d.className, (d as HTMLElement).style.getPropertyValue("--sheet-shade")])).toEqual(["mobile-sheet", ""]);
});

test("with reduced motion it opens and closes without sliding", { tag: "@chromium-only" }, async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const row = await openPopup(page);
  expect(await page.locator(`#${POPUP} .mobile-sheet-panel`).evaluate((p) => p.getAnimations().length), "no opening slide").toBe(0);
  await popup(page).evaluate((d) => {
    const w = window as unknown as { __left: boolean };
    w.__left = false;
    new MutationObserver(() => (w.__left ||= d.classList.contains("is-leaving"))).observe(d, { attributes: true });
  });
  await pressAt(page, await historyPoint(page));
  await expect(popup(page)).toBeHidden();
  expect(await page.evaluate(() => (window as unknown as { __left: boolean }).__left), "it never slid away").toBe(false);
  await expect(chips(row)).toHaveCount(0);
});

test("a pinch over the popup is not held, and the panel is not either", { tag: "@chromium-only" }, async ({ page }) => {
  await openPopup(page);
  const h = await historyPoint(page);
  const [px, py] = await handlePoint(page);
  expect(await touchmovePrevented(page, h.x, h.y, 2), "a pinch over the backdrop zooms").toBe(false);
  expect(await touchmovePrevented(page, px, py, 2), "a pinch over the panel zooms").toBe(false);
  expect(await touchmovePrevented(page, px, py), "one finger on the panel is the drag's").toBe(false);
});

test.describe("A mouse on the reaction popup", () => {
  test.skip(({ hasTouch }) => hasTouch, "a mouse drag, on the desktop engines");

  test("a mouse drag down closes it; a short or sideways one on the backdrop does not", async ({ page }) => {
    const row = await openPopup(page);
    const [x, y] = await handlePoint(page);
    await page.mouse.move(x, y);
    await page.mouse.down();
    // Inside the viewport: a mouse let go outside the window sends no
    // pointerup, and the drag springs back.
    await page.mouse.move(x, y + 60, { steps: 6 });
    await page.mouse.up();
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);

    await openReactionPopup(page, row);
    await panelAtRest(page, POPUP);
    const h = await historyPoint(page);
    await page.mouse.move(h.x, h.y);
    await page.mouse.down();
    await page.mouse.move(h.x, h.y + 20, { steps: 5 });
    // Held still before letting go: a slow drag, not a flick (the engines
    // send these steps a few ms apart).
    await page.waitForTimeout(150);
    await page.mouse.up();
    await settle(page);
    await expect(popup(page), "dragged short: springs back").toBeVisible();
    await panelAtRest(page, POPUP);
    expect(await panelOffset(page, POPUP)).toBe(0);

    // A sideways drag on the backdrop starts no drag, and the click that
    // ends it is no tap.
    await page.mouse.move(h.x, h.y);
    await page.mouse.down();
    await page.mouse.move(h.x + 60, h.y, { steps: 5 });
    await page.mouse.up();
    await settle(page);
    await expect(popup(page), "a sideways drag is no tap").toBeVisible();
    expect(await panelOffset(page, POPUP)).toBe(0);
  });

  test("a mouse drag up does not lift it", async ({ page }) => {
    await openPopup(page);
    const [x, y] = await handlePoint(page);
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x, y - 120, { steps: 6 });
    expect(await panelBottomGap(page, POPUP), "no gap under it").toBeLessThan(1);
    await page.mouse.up();
    await expect(popup(page)).toBeVisible();
  });
});

test.describe("The reaction popup's opening slide", () => {
  // `showModal()` focuses the first emoji while the slide still holds the
  // panel below the dialog. A dialog that can scroll is scrolled to reveal
  // it: WebKit kept that scroll for the whole slide, so the panel rose its
  // own height above the bottom edge and dropped back when the slide ended;
  // Chromium clamped it each frame, so the panel never slid at all.
  test("never scrolls the dialog or lifts the panel off the bottom edge", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "react target");
    const row = committedRow(page, "react target");
    await expect(row).toHaveCount(1);
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
    }, POPUP);
    await openReactionPopup(page, row);
    await panelAtRest(page, POPUP);
    await settle(page);
    const frames = await page.evaluate(() => (window as unknown as { __sheetFrames: { scrollTop: number; gap: number }[] }).__sheetFrames);
    expect(frames.length, "premise: frames were sampled while it was open").toBeGreaterThan(0);
    expect(Math.max(...frames.map((f) => f.scrollTop)), "the dialog never scrolls").toBe(0);
    expect(Math.max(...frames.map((f) => f.gap)), "never above the bottom edge").toBeLessThan(1);
    expect(
      await popup(page).evaluate((el) => getComputedStyle(el).overflowY),
      "the dialog is no scroll container",
    ).toBe("clip");
  });
});

test.describe("A finger on the reaction popup", () => {
  test.skip(({ browserName }) => browserName !== "chromium", "real touch input needs Chromium's input protocol");

  test("a slow drag down moves the panel with the finger, and springs back when let go short", async ({ page }) => {
    const row = await openPopup(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, 30] }, 20);
      const held = await panelOffset(page, POPUP);
      expect(held, "it follows the finger").toBeGreaterThan(27);
      expect(held).toBeLessThan(33);
      await fingers.lift();
    });
    await panelAtRest(page, POPUP);
    expect(await panelOffset(page, POPUP), "sprung back").toBe(0);
    await expect(popup(page)).toBeVisible();
    await expect(chips(row)).toHaveCount(0);
  });

  test("dragged past the threshold it slides away and closes, reacting with nothing", async ({ page }) => {
    const row = await openPopup(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, (fingers) => fingers.drag(x, y, 120));
    await expect(popup(page)).toBeHidden();
    await expect(chips(row)).toHaveCount(0);
    await expect(row.getByTestId("message-react-button"), "focus back on the +").toBeFocused();
  });

  test("a quick flick closes it even when short", async ({ page }) => {
    await openPopup(page);
    const [x, y] = await handlePoint(page);
    // One 30px move, then up: CDP spaces steps ~50ms apart, slower than a
    // real finger's flick, so one step is the honest flick here.
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, 30] }, 1);
      await fingers.lift();
    });
    await expect(popup(page)).toBeHidden();
  });

  test("a drag down that starts over the history drags it and closes it, and scrolls nothing", async ({ page }) => {
    await openPopup(page);
    const history = page.locator("#chat-scroll-container");
    const before = await history.evaluate((el) => el.scrollTop);
    const h = await historyPoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, h.x, h.y]);
      await fingers.move({ 1: [0, 60] }, 10);
      expect(await panelOffset(page, POPUP), "the panel moves with a finger above it").toBeGreaterThan(50);
      await fingers.move({ 1: [0, 90] }, 10);
      await fingers.lift();
    });
    await expect(popup(page)).toBeHidden();
    expect(await history.evaluate((el) => el.scrollTop), "the history did not move").toBe(before);
  });

  test("a drag down that starts on an emoji closes it without reacting", async ({ page }) => {
    const row = await openPopup(page);
    const signs = await callRiverTest(page, "messageSignsRecorded");
    const [x, y] = await choicePoint(page, 2);
    await withFingers(page, (fingers) => fingers.drag(x, y, 120));
    await expect(popup(page)).toBeHidden();
    await settle(page);
    await expect(chips(row)).toHaveCount(0);
    expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signs);
  });

  test("dragging up never lifts it above its rest position", async ({ page }) => {
    await openPopup(page);
    const [x, y] = await handlePoint(page);
    expect(await panelBottomGap(page, POPUP), "premise: at rest on the bottom edge").toBeLessThan(1);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, -100] }, 10);
      expect(await panelOffset(page, POPUP), "no upward offset").toBe(0);
      expect(await panelBottomGap(page, POPUP), "no gap under it").toBeLessThan(1);
      // Back down past the start: it follows from where the finger went down.
      await fingers.move({ 1: [0, 130] }, 10);
      expect(await panelOffset(page, POPUP)).toBeGreaterThan(20);
      await fingers.lift();
    });
    await panelAtRest(page, POPUP);
    expect(await panelOffset(page, POPUP)).toBe(0);
    await expect(popup(page)).toBeVisible();
  });

  test("a second finger cancels the drag", async ({ page }) => {
    await openPopup(page);
    const [x, y] = await handlePoint(page);
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [0, 30] }, 10);
      expect(await panelOffset(page, POPUP), "premise: dragging").toBeGreaterThan(20);
      await fingers.press([2, x + 60, y - 100]);
      await panelAtRest(page, POPUP);
      expect(await panelOffset(page, POPUP), "a pinch is no drag: back at rest").toBe(0);
      await fingers.move({ 1: [0, 100], 2: [0, -100] }, 10);
      await fingers.lift();
    });
    await settle(page);
    await expect(popup(page), "still open").toBeVisible();
  });

  test("a sideways swipe on the row scrolls it and leaves the panel where it is", async ({ page }) => {
    await openPopup(page);
    const box = (await palette(page).boundingBox())!;
    const [x, y] = [Math.round(box.x + box.width - 30), Math.round(box.y + box.height / 2)];
    await withFingers(page, async (fingers) => {
      await fingers.press([1, x, y]);
      await fingers.move({ 1: [-100, 6] }, 10);
      expect(await panelOffset(page, POPUP), "mid-swipe: the panel stays").toBe(0);
      await fingers.move({ 1: [-100, 0] }, 10);
      await fingers.lift();
    });
    await settle(page);
    expect(await palette(page).evaluate((el) => el.scrollLeft), "the row scrolled").toBeGreaterThan(50);
    expect(await panelOffset(page, POPUP)).toBe(0);
    await expect(popup(page)).toBeVisible();
  });
});

test.describe("The actions sheet's gestures", () => {
  const SHEET = "mobile-message-sheet";

  /// A received message, its actions sheet open and at rest.
  async function openSheet(page: Page): Promise<Locator> {
    await openRoomWithComposer(page);
    await deliver(page, "act target");
    const row = committedRow(page, "act target");
    await expect(row).toHaveCount(1);
    await openMessageActions(page, row);
    await panelAtRest(page, SHEET);
    return row;
  }

  test("it has no title or Close, and a tap outside slides it away", async ({ page }) => {
    const row = await openSheet(page);
    const sheet = page.getByTestId(SHEET);
    await expect(sheet).toHaveAccessibleName("Message options");
    await expect(sheet.getByRole("heading")).toHaveCount(0);
    await expect(page.getByTestId("mobile-message-actions-close")).toHaveCount(0);
    await pressAt(page, await historyPoint(page));
    await expect(sheet).toBeHidden();
    await expect(row.getByTestId("mobile-message-actions-entry"), "focus back on the entry").toBeFocused();
  });

  test.describe("with a finger", () => {
    test.skip(({ browserName }) => browserName !== "chromium", "real touch input needs Chromium's input protocol");

    test("a drag down from the handle closes it; a slow short one springs back", async ({ page }) => {
      await openSheet(page);
      const box = (await page.locator(`#${SHEET} .mobile-sheet-handle`).boundingBox())!;
      const [x, y] = [Math.round(box.x + box.width / 2), Math.round(box.y + box.height / 2)];
      await withFingers(page, async (fingers) => {
        await fingers.press([1, x, y]);
        await fingers.move({ 1: [0, 30] }, 20);
        await fingers.lift();
      });
      await panelAtRest(page, SHEET);
      expect(await panelOffset(page, SHEET), "sprung back").toBe(0);
      await expect(page.getByTestId(SHEET)).toBeVisible();
      await withFingers(page, (fingers) => fingers.drag(x, y, 150));
      await expect(page.getByTestId(SHEET)).toBeHidden();
    });

    test("a drag down that starts on Reply closes it without replying", async ({ page }) => {
      await openSheet(page);
      const box = (await page.getByTestId("mobile-message-action-reply").boundingBox())!;
      const [x, y] = [Math.round(box.x + box.width / 2), Math.round(box.y + box.height / 2)];
      await withFingers(page, (fingers) => fingers.drag(x, y, 150));
      await expect(page.getByTestId(SHEET)).toBeHidden();
      await settle(page);
      await expect(page.getByTestId("message-composer"), "no reply began").not.toContainText("Replying to");
    });
  });
});
