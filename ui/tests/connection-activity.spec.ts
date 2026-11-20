import { expect, Page, test } from "@playwright/test";
import { INVITATION_CODE } from "./dialog-cases";
import { visibleConnectionPill, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// Open River with its cached rooms loaded and the socket connected, and return
// the pill once it is idle.
async function openIdle(page: Page, url = "/") {
  await page.goto(url);
  await waitForApp(page);
  await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
  await callRiverTest(page, "setSyncStatus", "connected");
  const pill = visibleConnectionPill(page);
  await expect(pill).toHaveAttribute("aria-busy", "false");
  return pill;
}

test.describe("Connection activity", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("explains live room work, and clears when the work settles", { tag: "@chromium-only" }, async ({ page }) => {
    const pill = await openIdle(page);
    await expect(pill).toHaveAccessibleName("WebSocket connection status: Connected");
    await expect(pill.getByRole("tooltip", { includeHidden: true })).toHaveCount(0);

    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "migrating");
    await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");
    const details = pill.getByTestId("connection-status-details");
    await details.focus();
    await expect(details.getByRole("tooltip")).toHaveText("Loading your rooms");
    await expect(details.getByRole("tooltip")).toHaveCSS("pointer-events", "none");
    await callRiverTest(page, "releaseHeldLoadWorker");
    await expect(pill).toHaveAttribute("aria-busy", "false");

    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await callRiverTest(page, "showRoomInitialSync");
    await expect(pill).toHaveAttribute("data-busy-reason", "syncing-rooms");
    await callRiverTest(page, "setWalkRestoringRooms", true);
    await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");
    await callRiverTest(page, "setWalkRestoringRooms", false);
    await expect(pill).toHaveAttribute("data-busy-reason", "syncing-rooms");

    await callRiverTest(page, "showRoomSyncError", "Room not found", "Room unavailable", "Retry limit", false);
    await expect(pill).not.toHaveAttribute("data-busy-reason");
  });

  test("accepting a real invitation shows joining while subscribed room data is pending", { tag: "@chromium-only" }, async ({ page }) => {
    const pill = await openIdle(page, "/?invitation=" + INVITATION_CODE);
    await page.getByTestId("receive-invitation-nickname-input").fill("River tester");
    await page.getByTestId("receive-invitation-accept-button").click();
    await expect(pill).toHaveAttribute("data-busy-reason", "joining-room");
    await expect(pill.getByRole("tooltip", { includeHidden: true })).toHaveText("Joining the room");
  });

  // Below 48rem one panel shows at a time. While that panel's wave says the
  // rooms are loading, the pill keeps its status but leaves the dots to the wave.
  test("on a phone, the rooms-loading wave is the only loading animation", { tag: "@chromium-only" }, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await waitForApp(page);
    await callRiverTest(page, "setSyncStatus", "connected");
    const pill = visibleConnectionPill(page);
    const pillDots = pill.getByTestId("connection-activity-dots");
    const expectWaveOnly = async (wave: string) => {
      for (const state of ["loading", "migrating"] as const) {
        await callRiverTest(page, "setRoomsLoadState", state);
        await expect(page.getByTestId(`${wave}-${state}-dots`)).toBeVisible();
        await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");
        await expect(pill.getByTestId("connection-status-label")).toHaveText("Connected");
        await expect(pill.getByTestId("connection-status-dot")).toBeVisible();
        await expect(pillDots).toBeHidden();
      }
    };

    await expectWaveOnly("conversation-rooms");
    await callRiverTest(page, "setSyncStatus", "connecting");
    await expect(pill.getByTestId("connection-status-label")).toHaveText("Connecting");
    await expect(pillDots).toBeHidden();
    await callRiverTest(page, "setRoomsLoadState", "loaded");
    await expect(page.getByRole("heading", { name: "Welcome to River" })).toBeVisible();
    await expect(pillDots).toBeVisible();

    await callRiverTest(page, "setSyncStatus", "connected");
    await callRiverTest(page, "setRoomsLoadState", "loading");
    await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();
    await expectWaveOnly("room-list");
    await page.setViewportSize({ width: 1280, height: 800 });
    const railPill = page.getByTestId("rooms-rail").getByTestId("connection-status-indicator");
    await expect(railPill).toHaveAttribute("data-busy-reason", "loading-rooms");
    await expect(railPill.getByTestId("connection-activity-dots")).toBeVisible();
  });

  test("a refused listing clears the loading pill while cached rooms stay", { tag: "@chromium-only" }, async ({ page }) => {
    const pill = await openIdle(page);
    await callRiverTest(page, "beginListedLoad");
    await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");

    await callRiverTest(page, "refuseOutstandingList");
    await expect(pill).not.toHaveAttribute("data-busy-reason");
    await expect(page.getByTestId("room-list").locator('[data-testid^="room-item-"]').first()).toBeVisible();
  });

  test("a failed read of the stored rooms clears the loading pill while cached rooms stay", { tag: "@chromium-only" }, async ({ page }) => {
    const pill = await openIdle(page);
    const rows = page.getByTestId("room-list").locator('[data-testid^="room-item-"]');
    await expect(rows.first()).toBeVisible();
    const roomIds = await rows.evaluateAll(els => els.map(el => el.getAttribute("data-testid")));
    expect(roomIds.length).toBeGreaterThan(0);

    // A reconnect: the load is running (seen busy first, so an idle pill
    // below is the settled load, not the initial render).
    await callRiverTest(page, "beginListedLoad");
    await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");

    // The delegate lists only `rooms_data`, and reading it fails.
    await callRiverTest(page, "deliverStoredRoomsBlobListing");
    await expect(pill).not.toHaveAttribute("data-busy-reason");
    expect(await rows.evaluateAll(els => els.map(el => el.getAttribute("data-testid")))).toEqual(roomIds);
  });
});
