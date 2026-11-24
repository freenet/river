import { test, expect, Page } from "@playwright/test";
import { waitForApp, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";

// freenet/river#975: the Discover Rooms view. The example build has no
// network; `setDiscoverFixture` chooses what the index request is answered
// with, built through the real decode, verify and safe-search pipeline from a
// signed state holding: a public general room (featured), a room Atlas lists
// but that is private now, an unreachable room, and three safe search hides
// (unclassified, adult, malformed locator).

const SHOTS = "/home/ian/code/tmp/river-972-journey/discover";
const shot = (page: Page, name: string) => (process.env.DISCOVER_SHOTS ? page.screenshot({ path: `${SHOTS}/${name}.png` }) : undefined);

async function start(page: Page, fixture: "ready" | "hidden" | "empty" | "unavailable" | "loading") {
  await page.goto("/");
  await waitForApp(page);
  await page.waitForFunction(() => typeof (window as any).__riverTest?.setDiscoverFixture === "function");
  await callRiverTest(page, "setDiscoverFixture", fixture);
}

async function openFromHeader(page: Page) {
  // On a phone the rail is a panel of its own, reached from the conversation.
  await callRiverTest(page, "setMobileView", "rooms");
  await page.getByTestId("discover-rooms-button").click();
  await expect(page.getByTestId("discover")).toBeVisible();
}

const titles = (page: Page) => page.getByTestId("discover-card-title");

test("the empty room list offers Discover rooms as its action", async ({ page }) => {
  await start(page, "ready");
  await callRiverTest(page, "setRoomsLoadState", "loading");
  await callRiverTest(page, "setRoomsLoadState", "loaded");
  await callRiverTest(page, "setMobileView", "rooms");
  await expect(page.getByTestId("room-list-empty")).toHaveCount(1);
  await page.getByTestId("room-list-empty-discover").click();
  await expect(page.getByTestId("discover")).toBeVisible();
  await expect(page.getByTestId("discover-card")).not.toHaveCount(0);
});

test("the header button is there with rooms, and the list shows listed rooms but not hidden ones", async ({ page }) => {
  const panics = panicsOf(page);
  await start(page, "ready");
  // Always visible, not only when the list is empty.
  await expect(page.getByTestId("room-list-empty")).toHaveCount(0);
  await openFromHeader(page);

  await expect(titles(page)).toHaveText(["Atlas Open Room", "Atlas Quiet Room", "Atlas Faraway Room"]);
  for (const hidden of ["Hidden Unclassified Room", "Hidden Adult Room", "Hidden Malformed Room"]) {
    await expect(page.getByTestId("discover")).not.toContainText(hidden);
  }

  const cards = page.getByTestId("discover-card");
  // Featured first, with Atlas's text attributed to Atlas and never to the owner.
  await expect(cards.nth(0).getByTestId("discover-card-featured")).toBeVisible();
  await expect(cards.nth(0).getByTestId("discover-card-attribution")).toHaveText("Curated by Atlas");
  await expect(cards.nth(1).getByTestId("discover-card-attribution")).toHaveText("Summarised by Atlas (automated)");
  await expect(cards.nth(0).getByTestId("discover-card-snippet")).toContainText("decentralised software");
  // The owner's description is not on cards.
  await expect(page.getByTestId("discover")).not.toContainText("Anyone with the link can read this.");
  // A quiet note on the unreachable room only; it is still listed and never called dead.
  await expect(page.getByTestId("discover-card-unavailable")).toHaveCount(1);
  await expect(cards.nth(2).getByTestId("discover-card-unavailable")).toHaveText("may be unavailable");
  await expect(page.getByTestId("discover")).not.toContainText(/dead/i);
  await shot(page, `list-${test.info().project.name}`);
  expect(panics).toEqual([]);
});

test("search and tag chips narrow the list", async ({ page }) => {
  await start(page, "ready");
  await openFromHeader(page);
  const search = page.getByTestId("discover-search");

  await search.fill("gardening");
  await expect(titles(page)).toHaveText(["Atlas Quiet Room"]);
  await search.fill("software");
  await expect(titles(page)).toHaveText(["Atlas Open Room"]);
  await search.fill("history");
  await expect(titles(page)).toHaveText(["Atlas Faraway Room"]);
  await search.fill("nothing like this");
  await expect(page.getByTestId("discover-no-match")).toBeVisible();
  await expect(titles(page)).toHaveCount(0);
  await search.fill("");
  await expect(titles(page)).toHaveCount(3);

  // "chat" is on two rooms.
  await page.getByTestId("discover-tag").filter({ hasText: /^chat$/ }).click();
  await expect(titles(page)).toHaveText(["Atlas Open Room", "Atlas Quiet Room"]);
  await page.getByTestId("discover-tag").filter({ hasText: /^hobby$/ }).click();
  await expect(titles(page)).toHaveText(["Atlas Quiet Room"]);
  await shot(page, `search-${test.info().project.name}`);
  // Unselecting restores.
  await page.getByTestId("discover-tag").filter({ hasText: /^hobby$/ }).click();
  await page.getByTestId("discover-tag").filter({ hasText: /^chat$/ }).click();
  await expect(titles(page)).toHaveCount(3);
});

test("clicking a card opens the room read-only, with its owner description", async ({ page }) => {
  const panics = panicsOf(page);
  await start(page, "ready");
  await openFromHeader(page);
  await page.getByTestId("discover-card").filter({ hasText: "Atlas Open Room" }).click();

  await expect(page.getByTestId("discover")).toHaveCount(0);
  // The room's own name, not Atlas's snapshot title.
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Public Room");
  await expect(page.getByTestId("viewed-room")).toContainText("Anyone with the link can read this.");
  await expect(page.getByTestId("viewed-room-composer")).toBeDisabled();
  await shot(page, `opened-${test.info().project.name}`);
  expect(panics).toEqual([]);
});

test("a room Atlas lists that is private now opens as invite-only", async ({ page }) => {
  await start(page, "ready");
  await openFromHeader(page);
  await page.getByTestId("discover-card").filter({ hasText: "Atlas Quiet Room" }).click();
  await expect(page.getByTestId("viewed-room-invite-only")).toContainText("invite-only");
});

test("an unreachable room is still openable", async ({ page }) => {
  await start(page, "ready");
  await openFromHeader(page);
  await page.getByTestId("discover-card").filter({ hasText: "Atlas Faraway Room" }).click();
  await expect(page.getByTestId("viewed-room-not-found")).toBeVisible();
});

test("closing Discover returns to the user's rooms; picking a room leaves it", async ({ page }) => {
  await start(page, "ready");
  await openFromHeader(page);
  await page.getByTestId("discover-close").click();
  await expect(page.getByTestId("discover")).toHaveCount(0);

  await openFromHeader(page);
  const rooms = page.getByTestId("room-list").locator("li").first();
  await callRiverTest(page, "setMobileView", "rooms");
  await rooms.click();
  await expect(page.getByTestId("discover")).toHaveCount(0);
});

test("an error is not the empty state, and Retry recovers", async ({ page }) => {
  const panics = panicsOf(page);
  await start(page, "unavailable");
  await openFromHeader(page);
  await expect(page.getByTestId("discover-error")).toContainText("Discover is unavailable right now. Try again.");
  await expect(page.getByTestId("discover-empty")).toHaveCount(0);
  await expect(page.getByTestId("discover-card")).toHaveCount(0);
  await shot(page, `error-${test.info().project.name}`);

  await callRiverTest(page, "setDiscoverFixture", "ready");
  await page.getByTestId("discover-retry").click();
  await expect(page.getByTestId("discover-error")).toHaveCount(0);
  await expect(titles(page)).toHaveCount(3);
  expect(panics).toEqual([]);
});

test("an index with no rooms says public rooms will appear, and points at links", async ({ page }) => {
  await start(page, "empty");
  await openFromHeader(page);
  const empty = page.getByTestId("discover-empty");
  await expect(empty).toContainText("Public rooms will appear here soon.");
  await expect(empty).toContainText("Have a link to a room? Open it to read and join.");
  await expect(page.getByTestId("discover-error")).toHaveCount(0);
  await shot(page, `empty-${test.info().project.name}`);
});

test("rooms that safe search hides all show a distinct state", async ({ page }) => {
  await start(page, "hidden");
  await openFromHeader(page);
  await expect(page.getByTestId("discover-hidden")).toContainText("hidden by safe search");
  await expect(page.getByTestId("discover-empty")).toHaveCount(0);
  await expect(page.getByTestId("discover-card")).toHaveCount(0);
});

test("while the index has not answered it shows Loading, not empty", async ({ page }) => {
  await start(page, "loading");
  await openFromHeader(page);
  await expect(page.getByTestId("discover-loading")).toBeVisible();
  await expect(page.getByTestId("discover-empty")).toHaveCount(0);
  await expect(page.getByTestId("discover-error")).toHaveCount(0);
  await shot(page, `loading-${test.info().project.name}`);
});

test("browsing writes nothing: no delegate request, no rooms change, no browser storage", async ({ page }) => {
  await page.addInitScript(() => {
    const w = window as any;
    w.__storageWrites = 0;
    for (const store of [Storage.prototype]) {
      const set = store.setItem;
      store.setItem = function (...a: [string, string]) {
        w.__storageWrites++;
        return set.apply(this, a);
      };
      const rm = store.removeItem;
      store.removeItem = function (...a: [string]) {
        w.__storageWrites++;
        return rm.apply(this, a);
      };
    }
  });
  await start(page, "ready");
  await page.waitForTimeout(500);
  const roomsBefore = await page.getByTestId("room-list").locator("li").count();
  const delegateBefore = await callRiverTest(page, "delegateRequestsSent");
  const storageBefore = await page.evaluate(() => (window as any).__storageWrites as number);

  await openFromHeader(page);
  await page.getByTestId("discover-search").fill("chat");
  await page.getByTestId("discover-tag").first().click();
  await page.getByTestId("discover-close").click();
  await openFromHeader(page);
  await page.getByTestId("discover-card").first().click();
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await page.waitForTimeout(500);

  expect(await callRiverTest(page, "delegateRequestsSent")).toBe(delegateBefore);
  expect(await page.evaluate(() => (window as any).__storageWrites as number)).toBe(storageBefore);
  await page.getByTestId("viewed-room-close").click();
  expect(await page.getByTestId("room-list").locator("li").count()).toBe(roomsBefore);
});

test.describe("narrow screens", () => {
  test.use({ viewport: { width: 360, height: 740 } });

  test("Discover works at phone width and does not overflow sideways", async ({ page }) => {
    await start(page, "ready");
    await openFromHeader(page);
    await expect(titles(page)).toHaveCount(3);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow).toBeLessThanOrEqual(0);
    const card = await page.getByTestId("discover-card").first().boundingBox();
    expect(card!.x).toBeGreaterThanOrEqual(0);
    expect(card!.x + card!.width).toBeLessThanOrEqual(360);
    await shot(page, "phone-list");
    await page.getByTestId("discover-card").first().click();
    await expect(page.getByTestId("viewed-room")).toBeVisible();
  });
});
