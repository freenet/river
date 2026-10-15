import { test, expect, Locator, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";
import { callRiverTest } from "./river-test";
import { expectSettledAtBottom } from "./history-geometry";

// With no room selected, the no-room screen (Welcome, or the rooms load
// states) owns the conversation panel. The room history stays mounted for
// the next room but takes no space, so the phone's hamburger bar starts at the
// panel's top and the no-room content fills the rest.

const WELCOME = "Welcome to River";
// Fractional layout only; the defect is a band of hundreds of px.
const EDGE_PX = 1;

const history = (page: Page) => page.locator("#chat-scroll-container");

/// Where the no-room content sits in the conversation panel, whose direct
/// child it is, and where the phone's hamburger bar sits (null when hidden).
function layout(content: Locator) {
  return content.evaluate((el) => {
    const panel = el.parentElement!;
    const button = panel.querySelector<HTMLElement>('[data-testid="hamburger-rooms-button"]');
    const bar = button && button.getClientRects().length > 0 ? button.parentElement!.getBoundingClientRect() : null;
    const p = panel.getBoundingClientRect();
    const c = el.getBoundingClientRect();
    return {
      viewport: window.innerHeight,
      holdsHistory: panel.contains(document.getElementById("chat-scroll-container")),
      panel: { top: p.top, bottom: p.bottom },
      bar: bar && { top: bar.top, bottom: bar.bottom },
      content: { top: c.top, bottom: c.bottom },
    };
  });
}

async function expectFillsPanel(content: Locator, phone: boolean) {
  const g = await layout(content);
  expect(g.holdsHistory, "premise: measuring the conversation panel").toBe(true);
  expect(g.panel.top, "premise: the panel starts at the viewport's top").toBeCloseTo(0, 0);
  expect(Math.abs(g.panel.bottom - g.viewport), "premise: the panel reaches the viewport's bottom").toBeLessThanOrEqual(EDGE_PX);

  let top = g.panel.top;
  if (phone) {
    expect(g.bar, "premise: the phone shows the hamburger bar").not.toBeNull();
    expect(Math.abs(g.bar!.top - g.panel.top), "the hamburger bar starts at the panel's top").toBeLessThanOrEqual(EDGE_PX);
    top = g.bar!.bottom;
  } else {
    expect(g.bar, "premise: no hamburger bar on desktop").toBeNull();
  }
  expect(Math.abs(g.content.top - top), "the no-room content starts right below what is above it").toBeLessThanOrEqual(EDGE_PX);
  expect(Math.abs(g.content.bottom - g.panel.bottom), "the no-room content reaches the panel's bottom").toBeLessThanOrEqual(EDGE_PX);
}

for (const { label, viewport, phone } of [
  { label: "phone", viewport: { width: 390, height: 844 }, phone: true },
  { label: "desktop", viewport: { width: 1280, height: 800 }, phone: false },
]) {
  test.describe(`No-room screen fills the conversation panel (${label})`, () => {
    test.use({ viewport });

    test.beforeEach(async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      // PREMISE: the fixture seeds rooms and selects none.
      await expect(page.getByRole("heading", { name: WELCOME })).toBeVisible({ timeout: 10_000 });
      await expect(page.getByTestId("room-header-row")).toHaveCount(0);
    });

    test("Welcome", async ({ page }) => {
      await expectFillsPanel(page.getByRole("heading", { name: WELCOME }).locator(".."), phone);
      await expect(history(page), "the history stays mounted, hidden").toBeHidden();
    });

    test("Loading", async ({ page }) => {
      await callRiverTest(page, "setRoomsLoadState", "loading");
      const loading = page.getByTestId("conversation-rooms-loading");
      await expect(loading).toBeVisible({ timeout: 5_000 });
      await expectFillsPanel(loading, phone);
    });
  });
}

/// A room's unread badge, read while the room list may be behind the chat.
const unreadBadge = (page: Page, room: string) =>
  page.getByTestId("room-list").getByRole("button", { name: room, includeHidden: true }).getByTestId("room-unread-badge");

/// Open `room` from the no-room screen and check it opened at its newest
/// message with a usable composer.
async function openFromNoRoom(page: Page, room: string) {
  await selectRoom(page, room);
  await page.mouse.move(0, 0);
  await expect(history(page)).toBeVisible();
  await expectSettledAtBottom(page, `${room} should open at its newest message`);
  await expect(page.getByTestId("scroll-to-bottom"), "no Latest at the newest message").toHaveCount(0);
  const composer = page.getByTestId("message-input");
  await composer.fill("draft");
  await expect(composer).toHaveValue("draft");
  await composer.fill("");
}

test("a room opened from the no-room screen, and one opened after leaving it, opens at its newest message", async ({
  page,
}) => {
  await page.goto("/");
  await waitForApp(page);
  await expect(page.getByRole("heading", { name: WELCOME })).toBeVisible({ timeout: 10_000 });
  await expect(unreadBadge(page, "Team Chat Room"), "premise: Team Chat starts unread").toHaveCount(1);

  await openFromNoRoom(page, "Your Private Room");

  // Back to no room: leave it.
  await page.getByTestId("room-title-button").click();
  await page.getByTestId("edit-room-leave-button").click();
  await page.getByRole("button", { name: "Confirm Leave" }).click();
  await expect(page.getByRole("heading", { name: WELCOME })).toBeVisible({ timeout: 5_000 });

  await openFromNoRoom(page, "Team Chat Room");
  await expect(page.getByRole("heading", { name: "Team Chat Room" })).toBeVisible();

  await selectRoom(page, "Public Discussion Room");
  await expect(unreadBadge(page, "Team Chat Room"), "Team Chat was read at its newest message").toHaveCount(0);
});
