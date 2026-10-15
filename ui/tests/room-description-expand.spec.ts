import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom, settle } from "./example-room";
import { callRiverTest } from "./river-test";
import { expectSettledAtBottom, openRoomAtBottom, scrollMetrics } from "./history-geometry";

// freenet/river#718. A long room description is one truncated line until the
// reader asks for the rest. The chevron exists only when that line overflows.

const LONG_ROOM = "Public Discussion Room";
const SHORT_ROOM = "Team Chat Room";
// `?multiline-description=1`: a list of short lines, none wider than the
// header, so only the vertical clamp makes it overflow.
const MULTILINE_ROOM = "House Rules Room";
// Same fixture: one short line with inline code and an emoji, whose line box
// is a little taller than plain text's. It fits, so no chevron.
const INLINE_CODE_ROOM = "Inline Code Room";
// The app waits this long after a mouse click on the text before toggling
// (`DESCRIPTION_DOUBLE_CLICK_WAIT_MS`). Waits below add a wide margin.
const CLICK_WAIT_MS = 500;
const PAST_CLICK_WAIT_MS = CLICK_WAIT_MS + 700;

/// A point on the plain-text "Welcome" at the start of the long description.
async function welcomePoint(page: Page): Promise<{ x: number; y: number }> {
  const word = page.getByTestId("room-header-description").getByText("Welcome", { exact: false }).first();
  const box = await word.boundingBox();
  expect(box).not.toBeNull();
  return { x: box!.x + 12, y: box!.y + box!.height / 2 };
}

async function openLongRoom(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, LONG_ROOM);
  await expect(page.getByTestId("room-description-toggle")).toBeVisible();
}

async function descriptionHeight(page: Page): Promise<number> {
  const box = await page.getByTestId("room-header-description").boundingBox();
  expect(box).not.toBeNull();
  return box!.height;
}

test.describe("Room description expand", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("expanding grows the block, and another room starts collapsed", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, LONG_ROOM);

    const description = page.getByTestId("room-header-description");
    const toggle = page.getByTestId("room-description-toggle");
    await expect(description).toHaveAttribute("data-expanded", "false");
    await expect(toggle).toBeVisible();
    const collapsed = await description.boundingBox();
    expect(collapsed).not.toBeNull();

    // WCAG 2.2 target size, without making the header line taller.
    const toggleBox = await toggle.boundingBox();
    expect(toggleBox!.width).toBeGreaterThanOrEqual(24);
    expect(toggleBox!.height).toBeGreaterThanOrEqual(24);

    await toggle.click();
    await expect(description).toHaveAttribute("data-expanded", "true");
    const expanded = await description.boundingBox();
    expect(expanded!.height).toBeGreaterThan(collapsed!.height + 4);
    await expect(description.locator('a[href="https://freenet.org/atlas"]')).toBeVisible();

    await selectRoom(page, SHORT_ROOM);
    const short = page.getByTestId("room-header-description");
    await expect(short).toBeVisible();
    await expect(short).toHaveAttribute("data-expanded", "false");
    await expect(short).toContainText("Hi");
    // Let the overflow measurement land, or a count of 0 passes regardless.
    await settle(page);
    await expect(page.getByTestId("room-description-toggle")).toHaveCount(0);
  });

  test("coming back to a room shows its description collapsed", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, LONG_ROOM);
    const description = page.getByTestId("room-header-description");
    await page.getByTestId("room-description-toggle").click();
    await expect(description).toHaveAttribute("data-expanded", "true");

    await selectRoom(page, SHORT_ROOM);
    await expect(description).toContainText("Hi");
    await selectRoom(page, LONG_ROOM);
    await expect(description).toContainText("Welcome");
    await settle(page);
    await expect(description).toHaveAttribute("data-expanded", "false");
    await expect(page.getByTestId("room-description-toggle")).toHaveAttribute(
      "aria-expanded",
      "false"
    );
  });

  test("a multi-line description is clamped to one line and gets a chevron", async ({
    page,
  }) => {
    await page.goto("/?multiline-description=1");
    await waitForApp(page);
    await selectRoom(page, SHORT_ROOM);
    await settle(page);
    const oneLine = await descriptionHeight(page);

    await selectRoom(page, MULTILINE_ROOM);
    const description = page.getByTestId("room-header-description");
    await expect(description).toContainText("Rules:");
    // Premise: no line is wider than the header, so only the vertical
    // check can find the overflow.
    expect(
      await description.evaluate((el) => el.scrollWidth <= el.clientWidth + 1)
    ).toBe(true);
    const toggle = page.getByTestId("room-description-toggle");
    await expect(toggle).toBeVisible();
    // Collapsed is one line, the same height as a one-line description.
    expect(await descriptionHeight(page)).toBeCloseTo(oneLine, 0);

    await toggle.click();
    await expect(description).toHaveAttribute("data-expanded", "true");
    await expect(description.getByText("Stay on topic")).toBeVisible();
    expect(await descriptionHeight(page)).toBeGreaterThan(oneLine * 3);
  });

  test("a one-line description with inline code and an emoji gets no chevron", async ({
    page,
  }) => {
    await page.goto("/?multiline-description=1");
    await waitForApp(page);
    await selectRoom(page, INLINE_CODE_ROOM);
    const description = page.getByTestId("room-header-description");
    await expect(description.locator("code")).toHaveText("cargo make");
    await settle(page);
    await expect(page.getByTestId("room-description-toggle")).toHaveCount(0);
  });

  test("a slow double-click selects a word without expanding", async ({ page }) => {
    await openLongRoom(page);
    const description = page.getByTestId("room-header-description");
    const { x, y } = await welcomePoint(page);

    // Two clicks 350ms apart: slower than a quick double-click, well inside
    // common OS double-click times (Windows defaults to 500ms).
    await page.mouse.click(x, y);
    await page.waitForTimeout(350);
    await page.mouse.down({ clickCount: 2 });
    await page.mouse.up({ clickCount: 2 });
    await page.waitForTimeout(PAST_CLICK_WAIT_MS);
    await expect(description).toHaveAttribute("data-expanded", "false");

    // A single click still expands, once the double-click window has passed.
    await page.evaluate(() => window.getSelection()?.removeAllRanges());
    await page.mouse.click(x, y);
    await expect(description).toHaveAttribute("data-expanded", "true");
  });

  test("the chevron right after a click on the text leaves it expanded", async ({ page }) => {
    await openLongRoom(page);
    const description = page.getByTestId("room-header-description");
    const { x, y } = await welcomePoint(page);

    await page.mouse.click(x, y);
    await page.getByTestId("room-description-toggle").click();
    await expect(description).toHaveAttribute("data-expanded", "true");
    // The click on the text was still waiting. It must not toggle it back.
    await page.waitForTimeout(PAST_CLICK_WAIT_MS);
    await expect(description).toHaveAttribute("data-expanded", "true");
  });

  test("a click on the text does not expand the room after a switch away and back", async ({
    page,
  }) => {
    await openLongRoom(page);
    const description = page.getByTestId("room-header-description");
    const { x, y } = await welcomePoint(page);

    await page.mouse.click(x, y);
    await callRiverTest(page, "switchRoom", SHORT_ROOM);
    await expect(description).toContainText("Hi");
    await callRiverTest(page, "switchRoom", LONG_ROOM);
    await expect(description).toContainText("Welcome");
    await page.waitForTimeout(PAST_CLICK_WAIT_MS);
    await expect(description).toHaveAttribute("data-expanded", "false");
  });

  test("a tap on the text expands at once", async ({ page }) => {
    test.skip(!test.info().project.name.startsWith("mobile"), "needs a touchscreen");
    await openLongRoom(page);
    const description = page.getByTestId("room-header-description");
    const { x } = await welcomePoint(page);
    const line = (await description.boundingBox())!;
    // Low in the line: Chromium's touch adjustment moves a tap near the top
    // edge onto the Room details button just above.
    await page.touchscreen.tap(x, line.y + line.height - 3);
    // Well under the mouse wait: a finger does not double-click to select.
    await expect(description).toHaveAttribute("data-expanded", "true", {
      timeout: CLICK_WAIT_MS - 150,
    });
  });

  test("narrowing the window reveals the chevron", async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, LONG_ROOM);
    await settle(page);
    await expect(page.getByTestId("room-description-toggle")).toHaveCount(0);

    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByTestId("room-description-toggle")).toBeVisible();
  });

  // The header grows when the description expands, which shrinks the history
  // from above. A reader at the end must stay at the end (the end hold in
  // history-scrolling.md), and again when it collapses.
  test("a reader at the end stays there while the description expands and collapses", async ({
    page,
  }) => {
    await openRoomAtBottom(page, LONG_ROOM);
    const metrics = await scrollMetrics(page);
    expect(metrics.max, "premise: the history scrolls").toBeGreaterThan(100);

    const description = page.getByTestId("room-header-description");
    const toggle = page.getByTestId("room-description-toggle");
    const before = await description.boundingBox();
    await toggle.click();
    await expect(description).toHaveAttribute("data-expanded", "true");
    await settle(page);
    const after = await description.boundingBox();
    expect(after!.height, "premise: the header grew").toBeGreaterThan(before!.height + 4);
    await expectSettledAtBottom(page, "expanding the description should keep the reader at the end");

    await toggle.click();
    await expect(description).toHaveAttribute("data-expanded", "false");
    await settle(page);
    await expectSettledAtBottom(page, "collapsing the description should keep the reader at the end");
  });
});
