import { test, expect, Page, Locator } from "@playwright/test";

// Room header layout: the room title stays on the LEFT and is clickable; the
// room-details (i) and the notification bell form a right-aligned action
// group. Title and (i) open the SAME room-details modal (one shared handler
// in conversation.rs, so they cannot drift).

const ROOM = "Public Discussion Room";

// Sub-pixel layout rounding differs across engines; nothing here needs more.
const EPS = 2;

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

/**
 * Select ROOM at desktop width (the room list is always visible there), then
 * switch to the requested viewport. Scoped to the room list because once a
 * room is selected the header title button carries the room name as its
 * accessible name too.
 */
async function openRoomAt(page: Page, width: number, height: number) {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/");
  await waitForApp(page);
  const roomBtn = page.getByTestId("room-list").getByRole("button", { name: ROOM });
  await expect(roomBtn).toBeVisible({ timeout: 10_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: ROOM })).toBeVisible({ timeout: 5_000 });
  if (width !== 1280 || height !== 800) {
    await page.setViewportSize({ width, height });
  }
  await expect(page.getByTestId("room-info-button")).toBeVisible({ timeout: 5_000 });
}

async function box(locator: Locator) {
  const b = await locator.boundingBox();
  expect(b, "element should have a layout box").not.toBeNull();
  return b!;
}

const right = (b: { x: number; width: number }) => b.x + b.width;

/** Assertions that hold at every width. */
async function expectTitleLeftIconsRight(page: Page) {
  const row = await box(page.getByTestId("room-header-row"));
  const title = await box(page.getByTestId("room-title-button"));
  const info = await box(page.getByTestId("room-info-button"));
  const bell = await box(page.getByTestId("notification-bell-button"));

  // Title on the left half, icons after it, (i) before the bell.
  expect(title.x).toBeLessThan(row.x + row.width / 2);
  expect(info.x).toBeGreaterThanOrEqual(right(title) - EPS);
  expect(bell.x).toBeGreaterThanOrEqual(right(info) - EPS);

  // Nothing in the header row spills sideways.
  const rowOverflows = await page
    .getByTestId("room-header-row")
    .evaluate((el) => el.scrollWidth > el.clientWidth);
  expect(rowOverflows).toBe(false);
  const pageOverflows = await page.evaluate(
    () => document.documentElement.scrollWidth > document.documentElement.clientWidth
  );
  expect(pageOverflows).toBe(false);

  return { row, title, info, bell };
}

test.describe("Room header layout — desktop 1280px", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("title is left; (i) and bell are right-aligned", async ({ page }) => {
    await openRoomAt(page, 1280, 800);

    const { row, title, info, bell } = await expectTitleLeftIconsRight(page);
    expect(Math.abs(right(bell) - right(row))).toBeLessThanOrEqual(EPS);

    // The regression this guards: with the icons hugging the title, a short
    // room name left them mid-row. There must be real space in between.
    expect(info.x - right(title)).toBeGreaterThan(40);
  });

  test("clicking the title and clicking the (i) open the same room-details modal", async ({
    page,
  }) => {
    await openRoomAt(page, 1280, 800);
    const modal = page.getByTestId("edit-room-modal");

    await page.getByTestId("room-title-button").click();
    await expect(modal).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole("heading", { name: /Room Details/i })).toBeVisible();
    await page.getByTestId("edit-room-close-button").click();
    await expect(modal).toHaveCount(0, { timeout: 15_000 });
    await expect(page.getByRole("heading", { name: /Room Details/i })).toHaveCount(0);

    const info = page.getByTestId("room-info-button");
    await expect(info).toBeVisible({ timeout: 15_000 });
    await expect(info).toHaveAttribute("title", "Room details");
    await expect(info).toHaveAttribute("aria-label", "Room details");
    await info.click();
    await expect(modal).toBeVisible({ timeout: 5_000 });
  });
});

test.describe("Room header layout — mobile 320px", () => {
  const vp = { width: 320, height: 568 };
  test.use({ viewport: vp });

  test("icons sit right, beside the members button, with no overflow", async ({ page }) => {
    await openRoomAt(page, vp.width, vp.height);

    const hamburger = await box(page.getByTestId("hamburger-rooms-button"));
    const members = await box(page.getByTestId("header-members-button"));
    const { row, title, bell } = await expectTitleLeftIconsRight(page);

    // Members keeps the far right; the icon group sits to its left, with a
    // real gap so a tap on one does not land on the other.
    expect(Math.abs(right(members) - right(row))).toBeLessThanOrEqual(EPS);
    expect(members.x - right(bell)).toBeGreaterThanOrEqual(4);

    // #402: the room-details title target stays clear of the hamburger.
    expect(title.x - right(hamburger)).toBeGreaterThanOrEqual(4);

    // Every header control is fully on-screen.
    for (const b of [hamburger, title, bell, members]) {
      expect(b.x).toBeGreaterThanOrEqual(0);
      expect(right(b)).toBeLessThanOrEqual(vp.width + EPS);
    }
  });
});
