import { test, expect, Page } from "@playwright/test";
import { registerHistoryGeometry } from "./history-scroll-geometry";
import { callRiverTest } from "./river-test";
import {
  AT_BOTTOM_EPSILON_PX,
  SCROLL_TO_LATEST_MARGIN_PX,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  endMinus,
  expectSettledAtBottom,
  openRoomAtBottom,
  readerScrollsWithoutGesture,
} from "./history-scroll-helpers";

// The button is presentation only. Arrivals never move the view, so a message
// that lands below a reader at the end has to reveal the button. The observer
// callback is deferred, so visibility is polled.

const SHORT = "New short message: are you there?";

test.use({ viewport: { width: 900, height: 700 } });

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
});

async function buttonShown(page: Page) {
  await expect
    .poll(async () => page.getByTestId("scroll-to-bottom").isVisible(), {
      timeout: 5_000,
      message: "the scroll-to-latest button should be visible",
    })
    .toBe(true);
}

test("an arrival below the view shows the button", async ({ page }) => {
  const kinds: { name: string; arrive: (page: Page) => Promise<void> }[] = [
    {
      name: "a short message",
      arrive: (page) => deliver(page, SHORT),
    },
    {
      name: "a join event",
      arrive: async (page) => {
        await callRiverTest(page, "appendJoinEvent");
        await expect(page.getByText("joined the room").last()).toBeVisible({ timeout: 5_000 });
      },
    },
    {
      name: "a burst of three short messages",
      arrive: async (page) => {
        for (const n of [1, 2, 3]) await deliver(page, `burst ${n}`);
      },
    },
  ];
  for (const kind of kinds) {
    await openRoomAtBottom(page, "Team Chat Room");
    await kind.arrive(page);
    expect(
      await distanceFromBottom(page),
      `premise: ${kind.name} is not followed into view`,
    ).toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
    await buttonShown(page);
  }
});

test("an offset inside the bottom padding does not show the button", async ({ page }) => {
  await openRoomAtBottom(page, "Team Chat Room");
  await readerScrollsWithoutGesture(page, await endMinus(page, SCROLL_TO_LATEST_MARGIN_PX - 6));
  await afterLayoutSettles(page);
  for (let i = 0; i < 5; i++) {
    await page.waitForTimeout(100);
    expect(
      await page.getByTestId("scroll-to-bottom").count(),
      "only padding is below the view, so the button stays hidden",
    ).toBe(0);
  }
});

test("the margin covers only padding", async ({ page }) => {
  await openRoomAtBottom(page, "Team Chat Room");
  const belowRow = await page.evaluate(() => {
    const container = document.getElementById("chat-scroll-container")!;
    const row = [...container.querySelectorAll<HTMLElement>('[id^="msg-"]')].at(-1)!;
    return (
      container.getBoundingClientRect().bottom -
      row.getBoundingClientRect().bottom +
      (container.scrollHeight - container.scrollTop - container.clientHeight)
    );
  });
  expect(
    belowRow,
    "the margin is the content's bottom padding (py-4) plus rounding; a larger gap means real content can hide with the button",
  ).toBeLessThanOrEqual(SCROLL_TO_LATEST_MARGIN_PX);
});

test("clicking it hides it once at the end", async ({ page }) => {
  await openRoomAtBottom(page, "Team Chat Room");
  await deliver(page, SHORT);
  await buttonShown(page);
  await page.getByTestId("scroll-to-bottom").click();
  await expectSettledAtBottom(page, "the button should reach the end");
  await expect
    .poll(async () => page.getByTestId("scroll-to-bottom").count(), {
      timeout: 5_000,
      message: "the button should hide once the end is in view",
    })
    .toBe(0);
});
