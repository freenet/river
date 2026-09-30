import { test, expect } from "@playwright/test";
import { resolveColor, selectRoom, waitForApp } from "./example-room";

test("hovering a group's author header lights its whole first row, and the band stays while that row's picker is open", async ({ page }) => {
  test.skip(
    await page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches),
    "no band on touch"
  );
  // Deterministic groups; the other rooms' example data is random.
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
  // Rows bleed 8px into the gutter by design; nothing may overflow the scroller.
  expect(await page.evaluate(() => { const c = document.getElementById("chat-scroll-container")!; return c.scrollWidth - c.clientWidth; })).toBeLessThanOrEqual(0);

  // The picker is not inside the row, so moving away ends the hover: the open picker's "+" keeps the band.
  await row.getByTestId("add-reaction-button").click();
  await page.mouse.move(0, 0);
  // Wait out the band's 0.15s fade: a fade in progress would still pass the check.
  await row.evaluate((el) => Promise.all(el.getAnimations().map((a) => a.finished)));
  expect(await bg()).toEqual(await resolveColor(page, "var(--color-row-hover)"));
});
