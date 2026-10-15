import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom, settle } from "./example-room";
import { expectSettledAtBottom, openRoomAtBottom, scrollMetrics } from "./history-geometry";

// freenet/river#718. A long room description is one truncated line until the
// reader asks for the rest. The chevron exists only when that line overflows.

const LONG_ROOM = "Public Discussion Room";
const SHORT_ROOM = "Team Chat Room";
// `?multiline-description=1`: a list of short lines, none wider than the
// header, so only the vertical clamp makes it overflow.
const MULTILINE_ROOM = "House Rules Room";

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

  test("double-clicking a word selects it without expanding", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, LONG_ROOM);
    const description = page.getByTestId("room-header-description");
    await expect(page.getByTestId("room-description-toggle")).toBeVisible();

    // "Welcome" is plain text, not a link.
    const word = description.getByText("Welcome", { exact: false }).first();
    const box = await word.boundingBox();
    expect(box).not.toBeNull();
    await page.mouse.dblclick(box!.x + 12, box!.y + box!.height / 2);
    await settle(page);
    await expect(description).toHaveAttribute("data-expanded", "false");
    expect(await page.evaluate(() => window.getSelection()?.toString().trim())).not.toBe("");

    // A single click still expands, once the double-click window has passed.
    await page.evaluate(() => window.getSelection()?.removeAllRanges());
    await page.mouse.click(box!.x + 12, box!.y + box!.height / 2);
    await expect(description).toHaveAttribute("data-expanded", "true");
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
