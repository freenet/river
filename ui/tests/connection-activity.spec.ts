import { expect, firefox, Locator, Page, test } from "@playwright/test";
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
    await callRiverTest(page, "setSyncStatus", "connected");
    const pill = visibleConnectionPill(page);
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

  test("a restoring walk keeps the pill busy when the current list was empty", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await callRiverTest(page, "setRoomsLoadState", "loaded");
    await callRiverTest(page, "setSyncStatus", "connected");
    await expect(page.getByTestId("room-list-empty")).toBeVisible();
    const pill = visibleConnectionPill(page);
    await expect(pill).toHaveAttribute("aria-busy", "false");

    await callRiverTest(page, "setWalkRestoringRooms", true);
    await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");
    await expect(pill).toHaveAttribute("aria-busy", "true");
    await expect(page.getByTestId("room-list-empty")).toBeVisible();

    await callRiverTest(page, "setWalkRestoringRooms", false);
    await expect(pill).toHaveAttribute("aria-busy", "false");
    expect(await pill.getAttribute("data-busy-reason")).toBeNull();
  });

  test("a refused listing clears the loading pill while cached rooms stay", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await callRiverTest(page, "setSyncStatus", "connected");
    const pill = visibleConnectionPill(page);
    await expect(pill).toHaveAttribute("aria-busy", "false");

    await callRiverTest(page, "beginListedLoad");
    await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");

    await callRiverTest(page, "refuseOutstandingList");
    await expect(pill).toHaveAttribute("aria-busy", "false");
    expect(await pill.getAttribute("data-busy-reason")).toBeNull();
    await expect(page.getByTestId("room-list").locator('[data-testid^="room-item-"]').first()).toBeVisible();
  });

  test("a failed read of the stored rooms clears the loading pill while cached rooms stay", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await callRiverTest(page, "setSyncStatus", "connected");
    const pill = visibleConnectionPill(page);
    await expect(pill).toHaveAttribute("aria-busy", "false");
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
    await expect(pill).toHaveAttribute("aria-busy", "false");
    expect(await pill.getAttribute("data-busy-reason")).toBeNull();
    expect(await rows.evaluateAll(els => els.map(el => el.getAttribute("data-testid")))).toEqual(roomIds);
  });

  // Open the Rooms rail, confirm loading and migration leave the dots to that
  // rail, then widen and confirm the pill's own dots come back.
  async function expectOneRoomsLoadingAnimation(page: Page, desktop: { width: number; height: number }) {
    await callRiverTest(page, "setSyncStatus", "connected");
    await callRiverTest(page, "setRoomsLoadState", "loading");
    await page.getByTestId("hamburger-rooms-button").filter({ visible: true }).click();

    const pill = visibleConnectionPill(page);
    for (const state of ["loading", "migrating"] as const) {
      await callRiverTest(page, "setRoomsLoadState", state);
      await expect(pill).toHaveAttribute("data-busy-reason", "loading-rooms");
      await expect(page.getByTestId(`room-list-${state}-dots`)).toBeVisible();
      await expect(pill.getByTestId("connection-activity-dots")).toBeHidden();
      await expect(pill.getByTestId("connection-status-label")).toHaveText("Connected");
      await expect(pill.getByTestId("connection-status-dot")).toBeVisible();
    }

    await callRiverTest(page, "setRoomsLoadState", "loading");
    await page.setViewportSize(desktop);
    const railPill = page.getByTestId("rooms-rail").getByTestId("connection-status-indicator");
    await expect(railPill).toHaveAttribute("data-busy-reason", "loading-rooms");
    await expect(railPill.getByTestId("connection-activity-dots")).toBeVisible();
  }

  // The single-panel Rooms layout follows Tailwind's `md` (48rem), which grows
  // with the browser's default font size. With a 20px default an 800px window
  // is still single-panel (48rem = 960px), and the footer must still leave the
  // dots to the rail's wave.
  test("with a larger default font, the single-panel rooms list still shows one loading animation", async ({ browserName }, testInfo) => {
    test.skip(browserName !== "firefox", "the default-font preference is set through Firefox");
    const browser = await firefox.launch({
      firefoxUserPrefs: {
        "font.size.variable.x-western": 20,
        "font.size.variable.x-unicode": 20,
      },
    });
    try {
      const context = await browser.newContext({
        baseURL: testInfo.project.use.baseURL,
        viewport: { width: 800, height: 900 },
      });
      const page = await context.newPage();
      await page.goto("/");
      await waitForApp(page);
      expect(await page.evaluate(() => matchMedia("(min-width: 48rem)").matches), "800px is below 48rem at 20px").toBe(false);
      await expectOneRoomsLoadingAnimation(page, { width: 1280, height: 900 });
    } finally {
      await browser.close();
    }
  });

  test("the mobile rooms list shows one loading animation", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/");
    await waitForApp(page);
    await expectOneRoomsLoadingAnimation(page, { width: 1280, height: 800 });
  });

  // The page's scheduler keeps running. Seek just the actual transitions on
  // the dot slot, including their CSS delays, before an engine can finish
  // them. Each transition starts from a real connection-state change.
  async function sampleTransition(slot: Locator, connecting: boolean, times: number[]) {
    return slot.evaluate(async (el, args) => {
      const pill = el.closest('[data-testid="connection-status-indicator"]')!;
      const dot = pill.querySelector('[data-testid="connection-status-dot"]')!;
      const label = pill.querySelector('[data-testid="connection-status-label"]')!;
      const clip = el.querySelector(".connection-activity-clip")!;
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
        const c = clip.getBoundingClientRect();
        const shown = [...el.querySelectorAll(".small-dots > span")].filter(span => {
          const r = span.getBoundingClientRect();
          return r.left >= c.left - 0.5 && r.right <= c.right + 0.5;
        }).length;
        const labelX = label.getBoundingClientRect().left;
        return {
          time,
          labelX,
          gap: labelX - dot.getBoundingClientRect().right,
          slotWidth: el.getBoundingClientRect().width,
          clipWidth: c.width,
          shown,
          opacity: Number(getComputedStyle(el).opacity),
        };
      });
    }, { connecting, times });
  }

  // While the slot pushes the label away from the status dot, the dots must be
  // what fills the space: the label never slides over an empty gap. Sampled
  // part-way through the open and the close, where a slot that reveals its
  // empty margin first showed no dots at all.
  test("the label never slides over an empty dot slot", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    const pill = visibleConnectionPill(page);
    await expect(pill).toHaveAttribute("aria-busy", "false");
    const slot = pill.getByTestId("connection-activity-dots");
    const restGap = await slot.evaluate(el => {
      const pill = el.closest('[data-testid="connection-status-indicator"]')!;
      const dot = pill.querySelector('[data-testid="connection-status-dot"]')!;
      const label = pill.querySelector('[data-testid="connection-status-label"]')!;
      return label.getBoundingClientRect().left - dot.getBoundingClientRect().right;
    });

    const opening = await sampleTransition(slot, true, [30, 60, 100, 150, 200, 250, 300]);
    const closing = await sampleTransition(slot, false, [750, 800, 850, 900]);
    for (const f of [...opening, ...closing]) {
      // The box that pushes the label is the box that shows the dots: no
      // empty part of the slot sits between them and the label.
      expect(Math.abs(f.slotWidth - f.clipWidth), `slot ${f.slotWidth}px vs visible ${f.clipWidth}px at ${f.time}ms`).toBeLessThanOrEqual(0.5);
      // Room for a dot and its spacing: a dot must be in it.
      if (f.gap - restGap >= 8 && f.opacity > 0.2) {
        expect(f.shown, `dots shown when the label is pushed ${Math.round(f.gap - restGap)}px at ${f.time}ms`).toBeGreaterThan(0);
      }
    }
    // The samples did cover a partly open slot, and the open slot shows all
    // five dots (its width fits them exactly).
    expect(opening.some(f => f.gap - restGap >= 8 && f.gap - restGap < 20)).toBe(true);
    expect(opening[opening.length - 1].shown).toBe(5);
  });

  for (const reduced of [false, true]) {
    test(`the pill opens immediately and can resume mid-fade (${reduced ? "reduced" : "normal"} motion)`, async ({ page }) => {
      await page.emulateMedia({ reducedMotion: reduced ? "reduce" : "no-preference" });
      await page.goto("/");
      await waitForApp(page);
      const pill = visibleConnectionPill(page);
      await expect(pill).toHaveAttribute("aria-busy", "false");
      const slot = pill.getByTestId("connection-activity-dots");
      const idleWidth = await slot.evaluate(el => el.getBoundingClientRect().width);
      expect(idleWidth).toBe(0);
      const [start, middle, open] = await sampleTransition(slot, true, [0, 150, 300]);
      expect(start.opacity).toBe(1);
      expect(middle.slotWidth).toBeGreaterThan(0);
      expect(open.slotWidth).toBeGreaterThan(0);
      if (reduced) {
        expect(start.slotWidth).toBe(open.slotWidth);
        expect(start.labelX).toBe(open.labelX);
      } else {
        expect(start.slotWidth).toBe(0);
        expect(middle.slotWidth).toBeLessThan(open.slotWidth);
        expect(middle.labelX).toBeGreaterThan(start.labelX);
        expect(middle.labelX).toBeLessThan(open.labelX);
      }

      const [held, fading] = await sampleTransition(slot, false, [600, 850]);
      expect(held.opacity).toBe(1);
      expect(held.slotWidth).toBe(open.slotWidth);
      expect(fading.opacity).toBeGreaterThan(0);
      expect(fading.opacity).toBeLessThan(1);
      if (reduced) expect(fading.slotWidth).toBe(open.slotWidth);
      const [resumed] = await sampleTransition(slot, true, [300]);
      expect(resumed.opacity).toBe(1);
      expect(resumed.slotWidth).toBe(open.slotWidth);
      const [closed] = await sampleTransition(slot, false, [1001]);
      expect(closed.opacity).toBe(0);
      expect(closed.slotWidth).toBe(0);
    });
  }
});
