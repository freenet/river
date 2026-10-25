import { test, expect } from "@playwright/test";
import { openRoomWithComposer, resolveColor } from "./example-room";

// The composer is one rounded box holding the emoji button, the draft and an
// icon Send button: the accent colours the icon, never a solid fill.
test("emoji, draft and an icon Send share one box, with no solid accent fill", async ({ page }) => {
  await openRoomWithComposer(page);
  const box = page.getByTestId("message-composer-box");
  const send = page.getByTestId("send-message-button");
  for (const part of [page.getByTestId("composer-emoji-button"), page.getByTestId("message-input"), send]) {
    await expect(box.locator(part)).toHaveCount(1);
  }
  await expect(send).toHaveAccessibleName("Send");
  await expect(send.locator("svg")).toHaveCount(1);
  await expect(page.getByPlaceholder("Type your message...")).toBeVisible();

  const m = await send.evaluate((el) => ({ bg: getComputedStyle(el).backgroundColor, color: getComputedStyle(el).color }));
  const [bg, colour, accent, link] = await Promise.all([
    resolveColor(page, m.bg),
    resolveColor(page, m.color),
    resolveColor(page, "var(--color-accent)"),
    resolveColor(page, "var(--color-link)"),
  ]);
  expect(bg, "Send is not a solid accent fill").not.toEqual(accent);
  expect(bg[3], "Send has no fill at rest").toBe(0);
  expect(colour, "the Send icon is the accent colour").toEqual(link);

  // The box is rounded and draws the border; the draft inside draws none.
  const radius = await box.evaluate((el) => parseFloat(getComputedStyle(el).borderTopLeftRadius));
  expect(radius).toBeGreaterThanOrEqual(10);
  const inputBorder = await page.getByTestId("message-input").evaluate((el) => getComputedStyle(el).borderTopColor);
  expect((await resolveColor(page, inputBorder))[3], "the draft's own border is transparent").toBe(0);
});
