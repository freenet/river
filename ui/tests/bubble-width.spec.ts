import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, selectRoom, waitForApp } from "./example-room";

// Bubble text is at most 65ch and never more than 75% of the message column
// (main.css `.msg-bubble`). Only its own reaction row may make a bubble wider.

const ROOM = "Your Private Room";

async function openRoom(page: Page, width: number) {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, ROOM);
  await page.setViewportSize({ width, height: 800 });
}

type Measured = {
  cap: number;
  slack: number;
  bubbles: { width: number; reactionRow: number }[];
};

async function measure(page: Page): Promise<Measured> {
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
    const bubbles = all
      .map((b) => {
        const row = b
          .closest('[id^="msg-"]')!
          .querySelector('[data-testid="message-reaction-row"]');
        return {
          width: b.getBoundingClientRect().width,
          reactionRow: row ? row.getBoundingClientRect().width : 0,
        };
      })
      .filter((b) => b.width > 0);
    // After a resize, iPhone-emulated WebKit resolves `cqi` up to 8px short
    // (the history scroller's scrollbar); a fresh load is exact, and
    // everywhere else the scrollbar takes no width and the slack is 1px.
    const scroller = document.getElementById("chat-scroll-container")!;
    const scrollbar = scroller.offsetWidth - scroller.clientWidth;
    return { cap: Math.min(ch65, column * 0.75), slack: 0.75 * scrollbar + 1, bubbles };
  });
}

for (const width of [1280, 320]) {
  test(`bubbles are capped at min(65ch, 75% of the column) @ ${width}px`, async ({ page }) => {
    await openRoom(page, width);
    // A bubble reaching the cap is how the test proves the cap binds at all.
    await expect
      .poll(async () => {
        const m = await measure(page);
        return m.bubbles.some((b) => b.width <= m.cap + 1 && b.width >= m.cap - m.slack);
      }, { message: "some bubble should be exactly at the cap" })
      .toBe(true);

    const m = await measure(page);
    for (const b of m.bubbles) {
      expect(
        b.width,
        `bubble ${b.width}px exceeds the cap ${m.cap}px and its reaction row ${b.reactionRow}px`
      ).toBeLessThanOrEqual(Math.max(m.cap, b.reactionRow) + 1);
    }
  });
}

test("a reaction row wider than its message widens the bubble", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);

  const input = page.getByTestId("message-input");
  await input.fill("k");
  await input.press("Enter");

  const bubble = page.getByTestId("message-bubble").filter({ hasText: /^\s*k\s*$/ }).last();
  await expect(bubble).toBeVisible();
  const row = bubble.locator("xpath=ancestor::*[starts-with(@id,'msg-')][1]");
  await row.scrollIntoViewIfNeeded();
  if (!(await page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches))) {
    await row.hover();
  }
  await row.getByTestId("add-reaction-button").click();
  await page.getByTestId("emoji-picker").getByRole("button").first().click();
  await expect(row.getByTestId("reaction-chip")).toHaveCount(1);

  const reactionRow = row.getByTestId("message-reaction-row");
  const bubbleBox = (await bubble.boundingBox())!;
  const rowBox = (await reactionRow.boundingBox())!;
  const chipBox = (await reactionRow.locator(":scope > *").first().boundingBox())!;
  expect(bubbleBox.width, "the bubble widens to its reaction row").toBeGreaterThanOrEqual(
    rowBox.width - 1
  );
  expect(rowBox.height, "the reaction row stays on one line").toBeLessThan(2 * chipBox.height);
});

// With `container-type` on `#chat-content`, Safari 26.0-26.4 left the room list 0x0 after a narrow-to-wide resize (WebKit 307984).
test("widening from a phone layout with messages on screen shows the room list", async ({
  page,
}) => {
  await openRoom(page, 390);
  await expect(page.getByTestId("message-bubble").first()).toBeVisible();
  await page.setViewportSize({ width: 1280, height: 800 });
  const list = page.getByTestId("room-list");
  await expect(list).toBeVisible();
  const box = (await list.boundingBox())!;
  expect(box.width).toBeGreaterThan(0);
  expect(box.height).toBeGreaterThan(0);
  await list.locator('[data-testid^="room-item-"]').first().click();
});
