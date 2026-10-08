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

/// Two animation frames, so a patch's effects and the layout they cause land.
export function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () => new Promise<void>((r) => requestAnimationFrame(() => requestAnimationFrame(() => r()))),
  );
}

/// Let a patch, its effects and any deferred mark land, so that a geometry or
/// unread count read afterwards is not just early.
export async function settle(page: Page) {
  await nextFrames(page);
  await page.waitForTimeout(300);
}

// Force the tab's visibility state: override the `document.hidden` and
// `document.visibilityState` getters and dispatch `visibilitychange`, the way
// Chromium, WebKit and Firefox do when the tab goes to the background or back.
export async function setTabVisibility(page: Page, state: "hidden" | "visible") {
  await page.evaluate((state) => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => state === "hidden",
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => state,
    });
    document.dispatchEvent(new Event("visibilitychange"));
  }, state);
}

/// The (N) of the title while the tab is hidden: unread across every room and
/// DM thread, the open one included. Hides the tab if it is not hidden
/// already, and lets the hide's mark land first.
export async function hiddenTitleCount(page: Page): Promise<number> {
  if (await page.evaluate(() => document.visibilityState !== "hidden")) {
    await setTabVisibility(page, "hidden");
  }
  await settle(page);
  const counted = /^\((\d+)\) /.exec(await page.title());
  return counted ? Number(counted[1]) : 0;
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
  await nextFrames(page);
}

/// A room's unread badge in the room list. Hidden while it is the current
/// room, so callers open another room first.
export function roomUnreadBadge(page: Page, roomName: string): Locator {
  return page.getByTestId("room-list").getByRole("button", { name: roomName }).getByTestId("room-unread-badge");
}

export function memberRows(page: Page): Locator {
  return page.getByTestId("member-list").locator('[data-testid^="member-item-"] button');
}

// Self owns "Your Private Room"; in "Public Discussion Room" self is an observer with no composer.
export async function openRoomWithComposer(page: Page) {
  await selectRoom(page, "Your Private Room");
  await expect(page.getByTestId("message-composer")).toBeVisible({ timeout: 5_000 });
}

// The edit form on an own (accent) message: its kebab on touch, its hover
// actions otherwise (freenet/river#402). Returns the edit textarea. Without a
// `message`, the first own message is scrolled into view first; a given
// `message` is used from wherever it already is.
export async function openOwnMessageEdit(page: Page, message?: Locator): Promise<Locator> {
  const ownRow = message ?? page.locator('[id^="msg-"]:has(.bg-accent)').first();
  await expect(ownRow).toBeVisible();
  if (!message) await ownRow.scrollIntoViewIfNeeded();
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

// Any CSS colour (tokens, color-mix, oklab) as sRGB [r, g, b, a], so engines compare equal.
export function resolveColor(page: Page, css: string): Promise<number[]> {
  return page.evaluate((value) => {
    const probe = document.createElement("div");
    probe.style.color = value;
    document.body.appendChild(probe);
    const computed = getComputedStyle(probe).color;
    probe.remove();
    const ctx = Object.assign(document.createElement("canvas"), { width: 1, height: 1 })
      .getContext("2d", { willReadFrequently: true })!;
    ctx.fillStyle = computed;
    ctx.fillRect(0, 0, 1, 1);
    const [r, g, b, a] = ctx.getImageData(0, 0, 1, 1).data;
    return [r, g, b, a / 255];
  }, css);
}
