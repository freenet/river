import { test, expect, Page, Locator } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

const EPS = 2; // sub-pixel rounding

async function openRoomAt(page: Page, viewport: { width: number; height: number }) {
  await page.setViewportSize(viewport);
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Public Discussion Room");
  await expect(page.getByTestId("room-info-button")).toBeVisible({ timeout: 5_000 });
}

const box = async (l: Locator) => {
  const b = await l.boundingBox();
  expect(b).not.toBeNull();
  return b!;
};
const right = (b: { x: number; width: number }) => b.x + b.width;

test.describe("Room header layout", { tag: "@chromium-only" }, () => {
  test("at 1280px (i) and the bell sit at the right edge", async ({ page }) => {
    await openRoomAt(page, { width: 1280, height: 800 });
    const row = await box(page.getByTestId("room-header-row"));
    const info = await box(page.getByTestId("room-info-button"));
    const bell = await box(page.getByTestId("notification-bell-button"));
    expect(info.x).toBeGreaterThan(row.x + row.width / 2);
    expect(Math.abs(right(bell) - right(row))).toBeLessThanOrEqual(EPS);
  });

  test("at 320px the icons sit beside the members button, clear of the hamburger", async ({ page }) => {
    await openRoomAt(page, { width: 320, height: 568 });
    const row = await box(page.getByTestId("room-header-row"));
    const hamburger = await box(page.getByTestId("hamburger-rooms-button"));
    const title = await box(page.getByTestId("room-title-button"));
    const bell = await box(page.getByTestId("notification-bell-button"));
    const members = await box(page.getByTestId("header-members-button"));
    expect(Math.abs(right(members) - right(row))).toBeLessThanOrEqual(EPS);
    expect(members.x - right(bell)).toBeGreaterThanOrEqual(4);
    // #402: reaching for the room list must not open room details.
    expect(title.x - right(hamburger)).toBeGreaterThanOrEqual(4);
  });
});
