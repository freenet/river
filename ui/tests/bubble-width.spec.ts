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
    const all = [...document.querySelectorAll('[data-testid="message-bubble"]')];
    // `ch` in the bubble's own font.
    const probe = document.createElement("div");
    probe.style.width = "65ch";
    all[0].appendChild(probe);
    const ch65 = probe.getBoundingClientRect().width;
    probe.remove();
    return all.map((b) => {
      // `cqi` resolves against the group's .msg-bubbles size container, its content box.
      const box = b.closest(".msg-bubbles")!;
      const cs = getComputedStyle(box);
      const column = box.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
      return {
        w: b.getBoundingClientRect().width,
        cap: Math.min(ch65, column * 0.75),
        chips: b.closest('[id^="msg-"]')!.querySelectorAll('[data-testid="reaction-chip"]').length,
      };
    });
  });
}

for (const width of [1440, 320]) {
  test(`bubbles are capped at min(65ch, 75% of the column) @ ${width}px`, async ({ page }, testInfo) => {
    test.skip(testInfo.project.name.startsWith("mobile-"), "sets its own viewport, so it would repeat chromium and webkit");
    await openRoom(page, width);
    // Reactions may widen a bubble past the cap (up to .msg-body's); nothing else may.
    await expect
      .poll(async () => {
        const free = (await measure(page)).filter((b) => b.chips === 0);
        return free.length ? Math.max(...free.map((b) => b.w - b.cap)) : Infinity;
      }, { message: "no bubble without reactions may pass the cap" })
      .toBeLessThanOrEqual(1);
    await expect
      .poll(async () => {
        const free = (await measure(page)).filter((b) => b.chips === 0);
        return free.some((b) => b.w >= 0.9 * b.cap);
      }, { message: "some bubble should reach the cap" })
      .toBe(true);
  });
}

for (const width of [1280, 320]) {
  test(`action controls stay in the column under a pile of reactions @ ${width}px`, async ({
    page,
  }) => {
    await openRoom(page, width);
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
      const control = row.getByTestId("message-action-cluster");
      const box = (await control.boundingBox())!;
      expect(box.x, `${text} control's left edge`).toBeGreaterThanOrEqual(scroller.x);
      expect(box.x + box.width, `${text} control's right edge`).toBeLessThanOrEqual(
        scroller.x + scroller.width
      );
    }
  });
}
