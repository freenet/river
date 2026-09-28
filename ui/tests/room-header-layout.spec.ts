import { test, expect, Page, Locator } from "@playwright/test";
import { waitForApp, selectListedRoom } from "./example-room";

const ROOM = "Public Discussion Room";
const EPS = 2; // sub-pixel rounding across engines
async function openRoomAt(page: Page, viewport: { width: number; height: number }) {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/");
  await waitForApp(page);
  await selectListedRoom(page, ROOM);
  await page.setViewportSize(viewport);
  await expect(page.getByTestId("room-info-button")).toBeVisible({ timeout: 5_000 });
}
const box = async (l: Locator) => {
  const b = await l.boundingBox();
  expect(b).not.toBeNull();
  return b!;
};
const right = (b: { x: number; width: number }) => b.x + b.width;

test(
  "at 1280px the title is on the left and (i) + bell sit at the right edge",
  { tag: "@chromium-only" },
  async ({ page }) => {
    await openRoomAt(page, { width: 1280, height: 800 });
    const row = await box(page.getByTestId("room-header-row"));
    const title = await box(page.getByTestId("room-title-button"));
    const info = await box(page.getByTestId("room-info-button"));
    const bell = await box(page.getByTestId("notification-bell-button"));
    expect(title.x).toBeLessThan(row.x + row.width / 2);
    expect(info.x).toBeGreaterThan(row.x + row.width / 2);
    expect(bell.x).toBeGreaterThanOrEqual(right(info) - EPS);
    expect(Math.abs(right(bell) - right(row))).toBeLessThanOrEqual(EPS);
  },
);

test(
  "the title and the (i) open the same room-details modal",
  { tag: "@chromium-only" },
  async ({ page }) => {
    await openRoomAt(page, { width: 1280, height: 800 });
    const modal = page.getByTestId("edit-room-modal");
    await page.getByTestId("room-title-button").click();
    await expect(modal).toBeVisible({ timeout: 5_000 });
    await page.getByTestId("edit-room-close-button").click();
    await expect(modal).toHaveCount(0, { timeout: 15_000 });
    await page.getByTestId("room-info-button").click();
    await expect(modal).toBeVisible({ timeout: 5_000 });
  },
);

test(
  "at 320px the icons sit beside the members button, clear of the hamburger, with no overflow",
  { tag: "@chromium-only" },
  async ({ page }) => {
    await openRoomAt(page, { width: 320, height: 568 });
    const row = await box(page.getByTestId("room-header-row"));
    const hamburger = await box(page.getByTestId("hamburger-rooms-button"));
    const title = await box(page.getByTestId("room-title-button"));
    const bell = await box(page.getByTestId("notification-bell-button"));
    const members = await box(page.getByTestId("header-members-button"));
    expect(Math.abs(right(members) - right(row))).toBeLessThanOrEqual(EPS);
    expect(members.x - right(bell)).toBeGreaterThanOrEqual(4);
    expect(title.x - right(hamburger)).toBeGreaterThanOrEqual(4); // #402: moved from mobile-touch-ux
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth),
    ).toBe(true);
  },
);
