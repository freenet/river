import { expect, Page } from "@playwright/test";

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
  if (!(await listed.isVisible())) {
    const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
    if ((await hamburger.count()) > 0) {
      await hamburger.click();
    } else {
      const vp = page.viewportSize();
      if (!vp) throw new Error("selectRoom needs a fixed viewport");
      await page.setViewportSize({ width: 1280, height: vp.height });
      await selectListedRoom(page, roomName);
      await page.setViewportSize(vp);
      // Let layout settle after the resize round trip (mobile-safari measured too early).
      await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
      return;
    }
  }
  await selectListedRoom(page, roomName);
}

// Self owns "Your Private Room"; in "Public Discussion Room" self is an observer with no composer.
export async function openRoomWithComposer(page: Page) {
  await selectRoom(page, "Your Private Room");
  await expect(page.getByTestId("message-composer")).toBeVisible({ timeout: 5_000 });
}

// Any CSS colour (tokens, color-mix, oklab) as sRGB [r, g, b, a], so engines compare equal.
export function resolveColor(page: Page, css: string): Promise<number[]> {
  return page.evaluate((value) => {
    const probe = document.createElement("div");
    probe.style.color = value;
    document.body.appendChild(probe);
    const computed = getComputedStyle(probe).color;
    probe.remove();
    const ctx = Object.assign(document.createElement("canvas"), { width: 1, height: 1 }).getContext(
      "2d",
      { willReadFrequently: true }
    )!;
    ctx.fillStyle = computed;
    ctx.fillRect(0, 0, 1, 1);
    const [r, g, b, a] = ctx.getImageData(0, 0, 1, 1).data;
    return [r, g, b, a / 255];
  }, css);
}
