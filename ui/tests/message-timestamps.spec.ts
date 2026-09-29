import { test, expect, Page } from "@playwright/test";
import { waitForApp, openRoomWithComposer } from "./example-room";

/// Two own messages 3 minutes apart (one group), at whole-minute clock times after every fixture message.
/// Returns their [time, text] pairs.
async function sendTwoOwnMessages(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
  const t0 = Math.ceil(Date.now() / 60_000) * 60_000 + 60 * 60_000;
  const sent = [[t0, "timestamp probe one"], [t0 + 3 * 60_000, "timestamp probe two"]] as const;
  for (const [t, text] of sent) {
    await page.clock.setFixedTime(t); // fakes Date only; timers (and so `defer`) keep running
    await page.getByTestId("message-input").fill(text);
    await page.getByTestId("send-message-button").click();
    await expect(page.getByText(text)).toBeVisible();
  }
  return sent;
}

const rowOf = (page: Page, text: string) =>
  page.locator('[id^="msg-"]', { has: page.getByTestId("message-bubble").filter({ hasText: text }) });

test("every message shows its own time at the bottom right of its bubble, without hover", async ({ page }) => {
  const sent = await sendTwoOwnMessages(page);
  await page.mouse.move(0, 0);
  for (const [t, text] of sent) {
    const row = rowOf(page, text);
    const time = row.getByTestId("message-time");
    await expect(time).toHaveCount(1);
    expect(Date.parse((await time.getAttribute("datetime")) ?? "")).toBe(t);
    expect(await time.getAttribute("title")).toBeTruthy();
    expect(await time.evaluate((el) => el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }))).toBe(true);
    const b = (await row.getByTestId("message-bubble").boundingBox())!;
    const tb = (await time.boundingBox())!;
    expect(tb.y).toBeGreaterThanOrEqual(b.y + b.height - 1);
    expect(Math.abs(tb.x + tb.width - (b.x + b.width))).toBeLessThanOrEqual(8);
  }
});
