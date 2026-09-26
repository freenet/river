import { expect, Locator, Page } from "@playwright/test";

export async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
  // App injects its stylesheets on first render; until they load every panel is visible and nothing is where it will be.
  await page.waitForFunction(
    () => {
      const links = Array.from(document.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"]'));
      return links.length > 0 && links.every((l) => l.sheet !== null);
    },
    undefined,
    { timeout: 30_000 }
  );
}

// Scoped to the room list: once a room is open the header title has the same name.
export async function selectListedRoom(page: Page, roomName: string) {
  const roomBtn = page.getByTestId("room-list").getByRole("button", { name: roomName });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: roomName })).toBeVisible({ timeout: 5_000 });
}

// Any viewport: the list if it is on screen, else the hamburger, else widen for the click.
export async function selectRoom(page: Page, roomName: string) {
  const listed = page.getByTestId("room-list").getByRole("button", { name: roomName });
  if (await listed.isVisible()) return selectListedRoom(page, roomName);

  const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
  if ((await hamburger.count()) > 0) {
    await hamburger.click();
    return selectListedRoom(page, roomName);
  }

  const vp = page.viewportSize();
  if (!vp) throw new Error("selectRoom needs a fixed viewport");
  await page.setViewportSize({ width: 1280, height: vp.height });
  await selectListedRoom(page, roomName);
  await page.setViewportSize(vp);
  // Let layout settle after the resize round trip (mobile-safari measured too early).
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
}

export function memberRows(page: Page): Locator {
  return page.getByTestId("member-list").locator('[data-testid^="member-item-"] button');
}

// Self owns "Your Private Room"; in "Public Discussion Room" self is an observer with no composer.
export async function openRoomWithComposer(page: Page) {
  await selectRoom(page, "Your Private Room");
  await expect(page.getByTestId("message-composer")).toBeVisible({ timeout: 5_000 });
}

// The edit form on the first own (accent) message: its kebab on touch, its hover
// actions otherwise (freenet/river#402). Returns the edit textarea.
export async function openOwnMessageEdit(page: Page): Promise<Locator> {
  const ownRow = page.locator('[id^="msg-"]:has(.bg-accent)').first();
  await expect(ownRow).toBeVisible();
  await ownRow.scrollIntoViewIfNeeded();
  if (await page.evaluate(() => window.matchMedia("(hover: none)").matches)) {
    await ownRow.getByTestId("message-kebab").click();
    await page.getByTestId("message-action-menu").getByRole("button", { name: /edit/i }).click();
  } else {
    await ownRow.getByTestId("message-bubble").hover();
    await ownRow.getByRole("button", { name: /edit/i }).click();
  }
  const editArea = page.locator('textarea[id^="edit-msg-"]');
  await expect(editArea).toBeVisible({ timeout: 5_000 });
  return editArea;
}
