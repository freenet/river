import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom, openRoomWithComposer } from "./example-room";

/// Two own messages 3 minutes apart (one group), at whole-minute clock times after every fixture message.
async function sendTwoMinutesApart(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
  const t0 = Math.ceil(Date.now() / 60_000) * 60_000 + 60 * 60_000;
  const t1 = t0 + 3 * 60_000;
  for (const [t, text] of [[t0, "timestamp probe one"], [t1, "timestamp probe two"]] as const) {
    await page.clock.setFixedTime(t); // fakes Date only; timers (and so `defer`) keep running
    await page.getByTestId("message-input").fill(text);
    await page.getByTestId("send-message-button").click();
    await expect(page.getByText(text)).toBeVisible();
  }
  return { t0, t1 };
}

const rowOf = (page: Page, text: string) =>
  page.locator('[id^="msg-"]', { has: page.getByTestId("message-bubble").filter({ hasText: text }) });

test("every message shows its own time at the bottom right of its bubble, without hover", async ({ page }) => {
  const { t0, t1 } = await sendTwoMinutesApart(page);
  await page.mouse.move(0, 0);
  for (const [t, text] of [[t0, "timestamp probe one"], [t1, "timestamp probe two"]] as const) {
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

test("a received group's author header holds no time; its rows do", { tag: "@chromium-only" }, async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  const header = page.getByTestId("message-group-header").first();
  await expect(header).toBeVisible();
  await expect(header.getByTestId("message-time")).toHaveCount(0);
  const firstRow = header.locator("xpath=ancestor::*[starts-with(@id,'msg-')][1]");
  await expect(firstRow.getByTestId("message-time")).toHaveCount(1);
});
