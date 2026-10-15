import { test, expect } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// freenet/river#718. A long room description is one truncated line until the
// reader asks for the rest. The chevron exists only when that line overflows.

const LONG_ROOM = "Public Discussion Room";
const SHORT_ROOM = "Team Chat Room";

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
    await expect(page.getByTestId("room-description-toggle")).toHaveCount(0);
  });

  test("narrowing the window reveals the chevron", async ({ page }) => {
    await page.setViewportSize({ width: 1400, height: 800 });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, LONG_ROOM);
    await expect(page.getByTestId("room-description-toggle")).toHaveCount(0);

    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByTestId("room-description-toggle")).toBeVisible();
  });
});
