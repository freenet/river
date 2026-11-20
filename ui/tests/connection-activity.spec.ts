import { expect, Locator, test } from "@playwright/test";
import { INVITATION_CODE } from "./dialog-cases";
import { visibleConnectionPill, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

test.describe("Connection activity", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("explains live room work, and clears when the work settles", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    const pill = visibleConnectionPill(page);
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
    const pill = visibleConnectionPill(page);
    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await callRiverTest(page, "setSyncStatus", "connected");
    await expect(pill).toHaveAttribute("aria-busy", "false");
    await page.getByTestId("receive-invitation-nickname-input").fill("River tester");
    await page.getByTestId("receive-invitation-accept-button").click();
    await expect(pill).toHaveAttribute("data-busy-reason", "joining-room");
    await expect(pill.getByRole("tooltip", { includeHidden: true })).toHaveText("Joining the room");
  });

  test("the tablet rail fits the idle error label on one line after activity settles", async ({ page }) => {
    await page.setViewportSize({ width: 960, height: 800 });
    await page.goto("/");
    await waitForApp(page);
    const pill = visibleConnectionPill(page);
    const dots = pill.getByTestId("connection-activity-dots");
    await callRiverTest(page, "setSyncStatus", "connecting");
    await expect(pill).toHaveAttribute("data-busy-reason", "connecting");
    await callRiverTest(page, "setSyncErrorReport", "Unexpected message", "Unexpected reply", "Unexpected reply");
    const label = pill.getByTestId("connection-status-label");
    await expect(label).toHaveText("Unexpected message");
    await expect(dots).toHaveCSS("opacity", "0");
    const lines = await label.evaluate(el => {
      const text = document.createRange();
      text.selectNodeContents(el);
      return text.getClientRects().length;
    });
    expect(lines).toBe(1);
    await page.setViewportSize({ width: 768, height: 800 });
    const rail = page.getByTestId("rooms-rail");
    expect(await rail.evaluate(el => el.scrollWidth - el.clientWidth)).toBe(0);
  });

  test("the mobile load-error footer paints a connection already in progress on mount", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await waitForApp(page);
    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await expect(page.getByTestId("message-composer")).toBeVisible();
    await callRiverTest(page, "setSyncStatus", "connecting");
    // The rooms rail is mounted but hidden in phone Chat. Losing the selected
    // room on a load error now mounts a second pill while the socket is already Connecting.
    await expect(page.getByTestId("rooms-rail").getByTestId("connection-status-indicator"))
      .toHaveAttribute("data-busy-reason", "connecting");
    const firstStyle = await page.evaluate(() => new Promise<{ width: number; opacity: string }>(resolve => {
      const observer = new MutationObserver(() => {
        const slot = document.querySelector('[data-testid="conversation-rooms-error"] [data-testid="connection-activity-dots"]');
        if (!slot) return;
        observer.disconnect();
        resolve({ width: slot.getBoundingClientRect().width, opacity: getComputedStyle(slot).opacity });
      });
      observer.observe(document.body, { childList: true, subtree: true });
      (window as unknown as { __riverTest: { setRoomsLoadState(s: string): void } }).__riverTest.setRoomsLoadState("failed");
    }));
    expect(firstStyle.width).toBeGreaterThan(0);
    expect(firstStyle.opacity).toBe("1");
  });

  test("the mobile room-loading wave supplies the dots while the footer keeps its connection status", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await waitForApp(page);
    const pill = visibleConnectionPill(page);
    await callRiverTest(page, "setRoomsLoadState", "loading");
    await expect(page.getByTestId("conversation-rooms-loading-dots")).toBeVisible();
    await expect(pill.getByTestId("connection-status-label")).toHaveText("Disconnected");
    await expect(pill.getByTestId("connection-activity-dots")).toBeHidden();
    const idleGap = await pill.evaluate(el => {
      const dot = el.querySelector('[data-testid="connection-status-dot"]')!;
      const label = el.querySelector('[data-testid="connection-status-label"]')!;
      return label.getBoundingClientRect().left - dot.getBoundingClientRect().right;
    });
    expect(idleGap).toBeCloseTo(8);

    await callRiverTest(page, "setSyncStatus", "connected");
    for (const state of ["loading", "migrating"] as const) {
      await callRiverTest(page, "setRoomsLoadState", state);
      await expect(page.getByTestId(`conversation-rooms-${state}-dots`)).toBeVisible();
      await expect(pill.getByTestId("connection-status-label")).toHaveText("Connected");
      await expect(pill.getByTestId("connection-status-dot")).toBeVisible();
      await expect(pill.getByTestId("connection-activity-dots")).toBeHidden();
    }
    await callRiverTest(page, "setSyncStatus", "connecting");
    await expect(pill.getByTestId("connection-status-label")).toHaveText("Connecting");
    await expect(pill.getByTestId("connection-activity-dots")).toBeHidden();
    await callRiverTest(page, "setRoomsLoadState", "loaded");
    await expect(page.getByRole("heading", { name: "Welcome to River" })).toBeVisible();
    await expect(pill.getByTestId("connection-activity-dots")).toBeVisible();
  });

  // Seek the slot and label together. The child dots keep their own animation
  // clock, independent of the transition triggered by the connection state.
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
      const transitions = [...el.getAnimations(), ...label.getAnimations()];
      for (const transition of transitions) transition.pause();
      return args.times.map(time => {
        for (const transition of transitions) transition.currentTime = time;
        return {
          layoutWidth: (el as HTMLElement).offsetWidth,
          visibleWidth: el.getBoundingClientRect().width,
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
      const pill = visibleConnectionPill(page);
      await expect(pill).toHaveAttribute("aria-busy", "false");
      const slot = pill.getByTestId("connection-activity-dots");
      const reservedWidth = await slot.evaluate(el => (el as HTMLElement).offsetWidth);
      expect(reservedWidth).toBeGreaterThan(0);
      // Startup may already be fading to idle when aria-busy first clears.
      await expect.poll(() => slot.evaluate(el => el.getBoundingClientRect().width)).toBe(0);
      const idleLabelX = await pill.getByTestId("connection-status-label")
        .evaluate(el => el.getBoundingClientRect().left);
      const [start, middle, open] = await sampleTransition(slot, true, [0, 150, 300]);
      expect(start.opacity).toBe(1);
      expect(middle.visibleWidth).toBeGreaterThan(0);
      expect(open.visibleWidth).toBeGreaterThan(0);
      expect(open.labelX).toBeGreaterThan(idleLabelX);
      if (reduced) {
        expect(start.visibleWidth).toBe(open.visibleWidth);
        expect(start.labelX).toBe(open.labelX);
      } else {
        expect(start.visibleWidth).toBe(0);
        expect(start.labelX).toBe(idleLabelX);
        expect(middle.visibleWidth).toBeLessThan(open.visibleWidth);
        expect(middle.labelX).toBeGreaterThan(start.labelX);
        expect(middle.labelX).toBeLessThan(open.labelX);
      }

      const [held, fading] = await sampleTransition(slot, false, [600, 850]);
      expect(held.opacity).toBe(1);
      expect(held.visibleWidth).toBe(open.visibleWidth);
      expect(held.labelX).toBe(open.labelX);
      expect(fading.opacity).toBeGreaterThan(0);
      expect(fading.opacity).toBeLessThan(1);
      if (reduced) {
        expect(fading.visibleWidth).toBe(open.visibleWidth);
        expect(fading.labelX).toBe(open.labelX);
      } else {
        expect(fading.labelX).toBeLessThan(open.labelX);
        expect(fading.labelX).toBeGreaterThan(idleLabelX);
      }
      const [resumed] = await sampleTransition(slot, true, [300]);
      expect(resumed.opacity).toBe(1);
      expect(resumed.visibleWidth).toBe(open.visibleWidth);
      expect(resumed.labelX).toBe(open.labelX);
      const [closed] = await sampleTransition(slot, false, [1001]);
      expect(closed.opacity).toBe(0);
      expect(closed.visibleWidth).toBe(0);
      expect(closed.labelX).toBe(idleLabelX);
      for (const sample of [start, middle, open, held, fading, resumed, closed]) {
        expect(sample.layoutWidth).toBe(reservedWidth);
      }
    });
  }
});
