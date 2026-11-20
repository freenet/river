import { expect, Locator, Page, test } from "@playwright/test";
import { INVITATION_CODE } from "./dialog-cases";
import { waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

const visiblePill = (page: Page) => page.locator('[data-testid="connection-status-indicator"]:visible');

test.describe("Connection activity", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("explains live room work, and clears when the work settles", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    const pill = visiblePill(page);
    await expect(pill).toHaveAttribute("aria-busy", "false"); // no-sync starts Disconnected
    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await callRiverTest(page, "setSyncStatus", "connected");
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
    await expect(pill).toHaveAttribute("aria-busy", "false");
    expect(await pill.getAttribute("data-busy-reason")).toBeNull();
  });

  test("accepting a real invitation shows joining while subscribed room data is pending", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/?invitation=" + INVITATION_CODE);
    await waitForApp(page);
    const pill = visiblePill(page);
    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await callRiverTest(page, "setSyncStatus", "connected");
    await expect(pill).toHaveAttribute("aria-busy", "false");
    await page.getByTestId("receive-invitation-nickname-input").fill("River tester");
    await page.getByTestId("receive-invitation-accept-button").click();
    await expect(pill).toHaveAttribute("data-busy-reason", "joining-room");
    await expect(pill.getByRole("tooltip", { includeHidden: true })).toHaveText("Joining the room");
  });

  test("the mobile no-room pill paints a connection already in progress on mount", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await waitForApp(page);
    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await expect(page.getByTestId("message-composer")).toBeVisible();
    await callRiverTest(page, "setSyncStatus", "connecting");
    // The rooms rail is mounted but hidden in phone Chat. Losing the selected
    // room now mounts a second pill while the socket is already Connecting.
    await expect(page.getByTestId("rooms-rail").getByTestId("connection-status-indicator"))
      .toHaveAttribute("data-busy-reason", "connecting");
    const firstStyle = await page.evaluate(() => new Promise<{ width: number; opacity: string }>(resolve => {
      const observer = new MutationObserver(() => {
        const slot = document.querySelector('[data-testid="conversation-rooms-loading"] [data-testid="connection-activity-dots"]');
        if (!slot) return;
        observer.disconnect();
        resolve({ width: slot.getBoundingClientRect().width, opacity: getComputedStyle(slot).opacity });
      });
      observer.observe(document.body, { childList: true, subtree: true });
      (window as unknown as { __riverTest: { setRoomsLoadState(s: string): void } }).__riverTest.setRoomsLoadState("loading");
    }));
    expect(firstStyle.width).toBeGreaterThan(0);
    expect(firstStyle.opacity).toBe("1");
  });

  // The page's scheduler keeps running. Seek just the actual transitions on
  // the dot slot, including their CSS delays, before an engine can finish
  // them. Each transition starts from a real connection-state change.
  async function sampleTransition(slot: Locator, connecting: boolean, times: number[]) {
    return slot.evaluate(async (el, args) => {
      const pill = el.closest('[data-testid="connection-status-indicator"]')!;
      const label = pill.querySelector('[data-testid="connection-status-label"]')!;
      const changed = new Promise<void>(resolve => {
        const observer = new MutationObserver(() => {
          if (pill.getAttribute("aria-busy") === String(args.connecting)) {
            observer.disconnect();
            resolve();
          }
        });
        observer.observe(pill, { attributes: true, attributeFilter: ["aria-busy"] });
      });
      const hooks = (window as unknown as { __riverTest: { setSyncStatus(s: string): void } }).__riverTest;
      hooks.setSyncStatus(args.connecting ? "connecting" : "disconnected");
      await changed;
      const transitions = el.getAnimations();
      for (const transition of transitions) transition.pause();
      return args.times.map(time => {
        for (const transition of transitions) transition.currentTime = time;
        return {
          width: el.getBoundingClientRect().width,
          labelX: label.getBoundingClientRect().left,
          opacity: Number(getComputedStyle(el).opacity),
        };
      });
    }, { connecting, times });
  }

  for (const reduced of [false, true]) {
    test(`the pill opens immediately and can resume mid-fade (${reduced ? "reduced" : "normal"} motion)`, async ({ page }) => {
      await page.emulateMedia({ reducedMotion: reduced ? "reduce" : "no-preference" });
      await page.goto("/");
      await waitForApp(page);
      const pill = visiblePill(page);
      await expect(pill).toHaveAttribute("aria-busy", "false");
      const slot = pill.getByTestId("connection-activity-dots");
      const idleWidth = await slot.evaluate(el => el.getBoundingClientRect().width);
      expect(idleWidth).toBe(0);
      const [start, middle, open] = await sampleTransition(slot, true, [0, 150, 300]);
      expect(start.opacity).toBe(1);
      expect(middle.width).toBeGreaterThan(0);
      expect(open.width).toBeGreaterThan(0);
      if (reduced) {
        expect(start.width).toBe(open.width);
        expect(start.labelX).toBe(open.labelX);
      } else {
        expect(start.width).toBe(0);
        expect(middle.width).toBeLessThan(open.width);
        expect(middle.labelX).toBeGreaterThan(start.labelX);
        expect(middle.labelX).toBeLessThan(open.labelX);
      }

      const [held, fading] = await sampleTransition(slot, false, [600, 850]);
      expect(held.opacity).toBe(1);
      expect(held.width).toBe(open.width);
      expect(fading.opacity).toBeGreaterThan(0);
      expect(fading.opacity).toBeLessThan(1);
      if (reduced) expect(fading.width).toBe(open.width);
      const [resumed] = await sampleTransition(slot, true, [300]);
      expect(resumed.opacity).toBe(1);
      expect(resumed.width).toBe(open.width);
      const [closed] = await sampleTransition(slot, false, [1001]);
      expect(closed.opacity).toBe(0);
      expect(closed.width).toBe(0);
    });
  }
});
