import { test, expect, Page } from "@playwright/test";
import { CLOSE_VIEWER_LABEL, closeViewer, waitForApp, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";

// The room opened from a link on a phone: the rail and the members list are
// separate panels there, so leaving, reading the members and writing each need
// their own control. Runs on the mobile projects only.

async function openOpenRoom(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
  const key = await callRiverTest(page, "viewableRoomKey", "open");
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  test.skip((page.viewportSize()?.width ?? 1000) >= 768, "phone layout only");
});

test("a visitor on a phone types, answers the name prompt and lands in the room", async ({ page }) => {
  const panics = panicsOf(page);
  await openOpenRoom(page);
  await expect(page.getByTestId("viewed-room-composer")).toBeEnabled();
  await page.getByTestId("viewed-room-composer").fill("Hello from a phone");
  await page.getByTestId("viewed-room-send").click();
  const input = page.getByTestId("viewed-room-nickname-input");
  await expect(input).toBeVisible();
  const box = await page.getByTestId("viewed-room-nickname-continue").boundingBox();
  expect(box!.height).toBeGreaterThanOrEqual(44);
  await input.fill("Phone Pat");
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  await expect(page.getByTestId("message-composer").first()).toBeVisible();
  expect(await callRiverTest(page, "roomMessageCount", "Hello from a phone")).toBe(1);
  expect(await callRiverTest(page, "currentRoomSelfNickname")).toBe("Phone Pat");
  expect(panics).toEqual([]);
});

test("on a phone the header x leaves, the members button shows the members, the menu shows the viewed row", async ({ page }) => {
  const panics = panicsOf(page);
  await openOpenRoom(page);
  await expect(page.getByTestId("viewed-room-close-narrow")).toBeVisible();
  await expect(page.getByTestId("viewed-room-close-narrow")).toHaveAttribute("aria-label", CLOSE_VIEWER_LABEL);

  // Members: a panel of its own on a phone, with a way back.
  await page.getByTestId("viewed-room-members-button").click();
  await expect(page.getByTestId("members-rail")).toBeVisible();
  expect(await page.getByTestId("viewed-room-member").count()).toBeGreaterThan(0);
  await page.getByTestId("members-back-to-chat-button").click();
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();

  // The rooms panel shows the viewed room's row, and tapping it returns.
  await page.getByRole("button", { name: "Open room list" }).first().click();
  await expect(page.getByTestId("viewed-room-rail-row")).toBeVisible();
  await page.getByTestId("viewed-room-rail-name").click();
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();

  await closeViewer(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  expect(panics).toEqual([]);
});
