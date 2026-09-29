import { test, expect } from "@playwright/test";
import { resolveColor, selectRoom, waitForApp } from "./example-room";

test("hovering a group's author header lights its whole first row", async ({ page }) => {
  test.skip(
    await page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches),
    "no band on touch"
  );
  // Same-author pairs, so received groups have headers.
  await page.goto("/?deep-history-room=1");
  await waitForApp(page);
  await selectRoom(page, "Capped History Room");

  const header = page.getByTestId("message-group-header").last();
  const row = header.locator("xpath=ancestor::*[starts-with(@id,'msg-')][1]");
  const bg = async () =>
    resolveColor(page, await row.evaluate((el) => getComputedStyle(el).backgroundColor));

  await page.mouse.move(0, 0);
  await expect.poll(bg).toEqual(await resolveColor(page, "transparent"));
  await header.hover();
  await expect.poll(bg).toEqual(await resolveColor(page, "var(--color-row-hover)"));

  const rowBox = (await row.boundingBox())!;
  const historyBox = (await page.getByTestId("conversation-history").boundingBox())!;
  expect(rowBox.x).toBeLessThanOrEqual(historyBox.x);
  expect(rowBox.x + rowBox.width).toBeGreaterThanOrEqual(historyBox.x + historyBox.width);
});
