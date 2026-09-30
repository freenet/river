import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

async function openRoom(page: Page, width: number) {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
  await page.setViewportSize({ width, height: 800 });
}

async function measure(page: Page) {
  return page.evaluate(() => {
    const content = document.getElementById("chat-content")!;
    const cs = getComputedStyle(content);
    const column =
      content.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    const all = [...document.querySelectorAll('[data-testid="message-bubble"]')];
    // `ch` in the bubble's own font.
    const probe = document.createElement("div");
    probe.style.width = "65ch";
    all[0].appendChild(probe);
    const ch65 = probe.getBoundingClientRect().width;
    probe.remove();
    return {
      cap: Math.min(ch65, column * 0.75),
      bubbles: all.map((b) => ({
        w: b.getBoundingClientRect().width,
        chips: b.closest('[id^="msg-"]')!.querySelectorAll('[data-testid="reaction-chip"]').length,
      })),
    };
  });
}

for (const width of [1440, 320]) {
  test(`bubbles are capped at min(65ch, 75% of the column) @ ${width}px`, async ({ page }) => {
    await openRoom(page, width);
    // Reactions may widen a bubble past the cap (up to .msg-body's); nothing else may.
    await expect
      .poll(async () => {
        const m = await measure(page);
        const free = m.bubbles.filter((b) => b.chips === 0);
        return free.length ? Math.max(...free.map((b) => b.w)) - m.cap : Infinity;
      }, { message: "no bubble without reactions may pass the cap" })
      .toBeLessThanOrEqual(1);
    await expect
      .poll(async () => {
        const m = await measure(page);
        return m.bubbles.map((b) => b.w).some((w) => Math.abs(w - m.cap) <= 1);
      }, { message: "some bubble should sit at the cap" })
      .toBe(true);
  });
}

for (const width of [1280, 320]) {
  test(`action controls stay in the column under a pile of reactions @ ${width}px`, async ({
    page,
  }) => {
    await openRoom(page, width);
    const touch = await page.evaluate(() => matchMedia("(hover: none)").matches);
    const scroller = (await page.locator("#chat-scroll-container").boundingBox())!;
    // Received (left-aligned) and own (right-aligned); example data piles 24 chips on each.
    for (const text of [/keep it civil/, /welcome/]) {
      const row = page
        .locator('[id^="msg-"]')
        .filter({ has: page.getByTestId("message-bubble").filter({ hasText: text }) });
      await expect(row.getByTestId("reaction-chip")).toHaveCount(24);
      const bubbleBox = (await row.getByTestId("message-bubble").boundingBox())!;
      const reactionBox = (await row.getByTestId("message-reaction-row").boundingBox())!;
      expect(bubbleBox.width, `${text}: the bubble spans its reaction row`).toBeGreaterThanOrEqual(
        reactionBox.width - 1
      );
      const control = row.getByTestId(touch ? "message-kebab" : "message-hover-actions");
      const box = (await control.boundingBox())!;
      expect(box.x, `${text} control's left edge`).toBeGreaterThanOrEqual(scroller.x);
      expect(box.x + box.width, `${text} control's right edge`).toBeLessThanOrEqual(
        scroller.x + scroller.width
      );
    }
  });
}
