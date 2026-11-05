import { test, expect, Locator, Page } from "@playwright/test";
import { waitForApp, nextFrames, selectRoom } from "./example-room";
import { callRiverTest, SyncStatus } from "./river-test";
import { DM_PEER } from "./dm-thread";
import { captureClipboardWrites, clipboardWrites } from "./clipboard";

// Short connection and room-sync labels, explained at once on hover or focus,
// with "Click to copy details" copying a diagnostic report through the
// sandbox-safe clipboard helper and confirming in the app toast
// (`components/tooltip.rs`, `freenet_api/error_report.rs`).
//
// The hooks go through the same deferred `set_sync_status` the synchronizer
// uses. Which real failure maps to which label, the report format and the
// ordering of display writes across connection attempts are unit tests in
// `error_report.rs` and `app.rs`; this checks what the reader sees.

const PILL = '[data-testid="connection-status-indicator"]:visible';
const LONG_TOKEN = "x".repeat(400);

function pill(page: Page): Locator {
  return page.locator(PILL);
}

function pillLabel(page: Page): Locator {
  return pill(page).getByTestId("connection-status-label");
}

function tooltipOf(host: Locator): Locator {
  return host.locator("[role=tooltip]");
}

async function open(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await captureClipboardWrites(page);
}

async function setStatus(page: Page, status: SyncStatus) {
  await callRiverTest(page, "setSyncStatus", status);
}

/// Shown the moment the host is hovered: read once, no retrying wait.
async function expectShownAtOnce(card: Locator) {
  const style = await card.evaluate((el) => {
    const cs = getComputedStyle(el);
    return {
      visibility: cs.visibility,
      transition: cs.transitionDuration,
      animation: cs.animationName,
    };
  });
  expect(style).toEqual({ visibility: "visible", transition: "0s", animation: "none" });
}

async function withinViewport(page: Page, el: Locator) {
  const box = (await el.boundingBox())!;
  const vp = page.viewportSize()!;
  expect(box.x, "left edge on screen").toBeGreaterThanOrEqual(0);
  expect(box.x + box.width, "right edge on screen").toBeLessThanOrEqual(vp.width + 0.5);
  expect(box.y, "top edge on screen").toBeGreaterThanOrEqual(0);
  expect(box.y + box.height, "bottom edge on screen").toBeLessThanOrEqual(vp.height + 0.5);
}

test.describe("Connection error details", () => {
  test("a closed connection reads Disconnected, explains on hover and copies on click", async ({
    page,
  }) => {
    await open(page);
    await setStatus(page, "error");

    await expect(pillLabel(page)).toHaveText("Disconnected");
    await expect(pill(page)).toHaveAttribute("aria-label", "WebSocket connection status");
    await expect(pill(page)).toHaveJSProperty("tagName", "BUTTON");
    const card = tooltipOf(pill(page));
    await expect(card).toBeHidden();
    await expect(pill(page)).toHaveAttribute("aria-describedby", (await card.getAttribute("id"))!);

    await pill(page).hover();
    await expectShownAtOnce(card);
    await expect(card).toContainText("Connection closed");
    await expect(card).toContainText("Click to copy details");
    await withinViewport(page, card);

    await pill(page).click();
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
    const [copied] = await clipboardWrites(page);
    expect(copied).toMatch(
      /^River: Disconnected\nConnection closed\nWebSocket error: request error: .*"source":"close".*\nTime: \d{4}-\d\d-\d\dT[^\n]+\nBuild: [^\n]+/
    );
  });

  test("keyboard focus explains and Enter copies", async ({ page }) => {
    await open(page);
    await setStatus(page, "error:The node went away");
    await expect(pillLabel(page)).toHaveText("Disconnected");

    // No pointer has been used on the page, so script focus is focus-visible.
    await pill(page).focus();
    await expectShownAtOnce(tooltipOf(pill(page)));
    await expect(tooltipOf(pill(page))).toContainText("The node went away");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
    expect((await clipboardWrites(page))[0]).toMatch(/^River: Disconnected\nThe node went away\n/);
  });

  test("the last error stays explained while reconnecting, until Connected", async ({ page }) => {
    await open(page);
    await setStatus(page, "error");
    await setStatus(page, "connecting");

    await expect(pillLabel(page)).toHaveText("Connecting...");
    await pill(page).hover();
    await expect(tooltipOf(pill(page))).toContainText("Connection closed");
    await pill(page).click();
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
    expect((await clipboardWrites(page))[0]).toMatch(/^River: Disconnected\nConnection closed\n/);

    await setStatus(page, "disconnected");
    await expect(pillLabel(page)).toHaveText("Disconnected");
    await expect(tooltipOf(pill(page))).toContainText("Connection closed");

    await setStatus(page, "connected");
    await expect(pillLabel(page)).toHaveText("Connected");
    await expect(pill(page)).toHaveJSProperty("tagName", "DIV");
    await expect(tooltipOf(pill(page))).toHaveCount(0);
  });

  test("display writes apply in order: an error then Connected leaves no error", async ({ page }) => {
    await open(page);
    await page.evaluate(() => {
      const hooks = (window as unknown as { __riverTest: Record<string, (s: string) => void> })
        .__riverTest;
      hooks.setSyncStatus("error");
      hooks.setSyncStatus("connected");
    });
    await expect(pillLabel(page)).toHaveText("Connected");
    await nextFrames(page);
    await expect(pillLabel(page)).toHaveText("Connected");
    await expect(tooltipOf(pill(page))).toHaveCount(0);
  });

  test("offline example data invents no error to explain", async ({ page }) => {
    await open(page);
    await expect(pillLabel(page)).toHaveText("Disconnected");
    await expect(pill(page)).toHaveJSProperty("tagName", "DIV");
    await expect(tooltipOf(pill(page))).toHaveCount(0);
  });

  test("an auth reload reads Refreshing... with the progress dot", async ({ page }) => {
    await open(page);
    await callRiverTest(
      page,
      "setSyncErrorReport",
      "Refreshing...",
      "Authentication expired. Refreshing the page.",
      "AUTH_TOKEN_INVALID"
    );
    await expect(pillLabel(page)).toHaveText("Refreshing...");
    await expect(pill(page).getByTestId("connection-status-dot")).toHaveClass(/(^|\s)bg-warning(\s|$)/);
    await pill(page).hover();
    await expect(tooltipOf(pill(page))).toContainText("Authentication expired. Refreshing the page.");
  });

  test("a long unbroken payload wraps and clamps to three lines", async ({ page }) => {
    await open(page);
    await setStatus(page, `error:${LONG_TOKEN}`);
    await pill(page).hover();
    const text = tooltipOf(pill(page)).locator(".tooltip-text");
    await expect(text).toBeVisible();
    const { height, lineHeight, scrollWidth, clientWidth } = await text.evaluate((el) => ({
      height: el.getBoundingClientRect().height,
      lineHeight: parseFloat(getComputedStyle(el).lineHeight),
      scrollWidth: el.scrollWidth,
      clientWidth: el.clientWidth,
    }));
    expect(height).toBeLessThanOrEqual(3 * lineHeight + 1);
    expect(height).toBeGreaterThan(2 * lineHeight);
    expect(scrollWidth, "the token wraps instead of overflowing").toBeLessThanOrEqual(clientWidth);
    await withinViewport(page, tooltipOf(pill(page)));
  });

  test("copying replaces an archive Undo; Un-archive stays available", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await open(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "appendDmsForPeer", DM_PEER.index, 3);
    const row = page.locator(".dm-rail-row-btn", { hasText: DM_PEER.name });
    await row.hover();
    await row.locator("xpath=..").locator(".dm-archive-btn").click();
    await expect(page.getByTestId("toast-action")).toHaveText("Undo");

    await setStatus(page, "error");
    await pill(page).click();
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
    await expect(page.getByTestId("toast-action")).toHaveCount(0);
    // Still archived, and the Archived panel still offers Un-archive.
    await expect(row).toHaveCount(0);
    await page.getByRole("button", { name: /^Archived \(1\)/ }).click();
    await expect(page.getByRole("button", { name: /Un-archive/ }).first()).toBeVisible();
  });
});

test.describe("Connection error details at 320px", () => {
  test.use({ viewport: { width: 320, height: 640 } });

  test("the explanation stays on screen and clear of the update banner", async ({ page }) => {
    await open(page);
    await callRiverTest(page, "showUpdateNotice", "countdown");
    await setStatus(page, `error:${LONG_TOKEN}`);
    await pill(page).hover();
    const card = tooltipOf(pill(page));
    await expect(card).toBeVisible();
    await withinViewport(page, card);
    for (const name of ["Reload", "Not now"]) {
      const button = page.getByRole("button", { name, exact: true });
      await expect(button).toBeVisible();
      const reachable = await button.evaluate((node) => {
        const r = node.getBoundingClientRect();
        const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return hit !== null && node.contains(hit);
      });
      expect(reachable, `${name} is not covered by the tooltip`).toBe(true);
    }
  });
});

test.describe("Room sync error details", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  async function failCurrentRoom(page: Page) {
    await open(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(
      page,
      "showRoomSyncError",
      "Room not found",
      "This room could not be found on the network — it may have been removed.",
      "Initial sync failed 10 times"
    );
  }

  test("the banner shows the label, explains and copies", async ({ page }) => {
    await failCurrentRoom(page);
    const banner = page.getByTestId("room-sync-error");
    await expect(banner).toContainText("Room not found");
    await expect(banner).toHaveJSProperty("tagName", "BUTTON");
    await banner.hover();
    await expectShownAtOnce(tooltipOf(banner));
    await expect(tooltipOf(banner)).toContainText("could not be found on the network");
    await banner.click();
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
    expect((await clipboardWrites(page))[0]).toMatch(
      /^River: Room not found\nThis room could not be found on the network — it may have been removed\.\nInitial sync failed 10 times\nTime: /
    );
  });

  test("the rail marker explains only, and its row still opens the room", async ({ page }) => {
    await failCurrentRoom(page);
    const marker = page.getByTestId("room-sync-error-marker");
    await expect(marker).toHaveAttribute("role", "img");
    await expect(marker).toHaveAttribute("aria-label", "Room sync failed: Room not found");
    await expect(marker).toHaveJSProperty("tagName", "SPAN");
    // Inside the row's button, never a button of its own.
    expect(await marker.evaluate((el) => el.closest("button") !== null)).toBe(true);
    await expect(marker.locator("button")).toHaveCount(0);
    await expect(tooltipOf(marker)).not.toContainText("Click to copy details");

    await marker.hover();
    await expectShownAtOnce(tooltipOf(marker));
    await expect(tooltipOf(marker)).toContainText("could not be found on the network");
    await withinViewport(page, tooltipOf(marker));

    await callRiverTest(page, "switchRoom", "Public Discussion Room");
    await expect(page.getByTestId("room-sync-error")).toHaveCount(0);
    await marker.click();
    await expect(page.getByTestId("room-sync-error")).toBeVisible();
    expect(await clipboardWrites(page)).toEqual([]);
  });
});

// Engines without CSS anchor positioning get the absolute fallback. Forced
// here on every engine by overriding the anchored rules, so the fallback is
// checked whatever the engine under test supports.
test.describe("Tooltip fallback without anchor positioning", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  // Undo exactly what the @supports block in main.css sets.
  async function forceFallback(page: Page) {
    await page.addStyleTag({
      content: `
        .tooltip-host > .tooltip-card {
          position: absolute !important; position-area: none !important;
          position-try-fallbacks: none !important;
          inset: auto auto 100% 0 !important;
        }
        .tooltip-host--framed > .tooltip-card {
          inset: auto 0.5rem 100% 0.5rem !important; width: auto !important; max-width: none !important;
        }
        .tooltip-host--framed.tooltip-host--below > .tooltip-card {
          inset: 100% 0.5rem auto 0.5rem !important;
        }`,
    });
  }

  /// The card lies inside the rail that clips it, so none of it is cut.
  async function insideRail(page: Page, card: Locator) {
    const rail = (await page.getByTestId("rooms-rail").boundingBox())!;
    const box = (await card.boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(rail.x);
    expect(box.x + box.width).toBeLessThanOrEqual(rail.x + rail.width + 0.5);
    expect(box.y).toBeGreaterThanOrEqual(rail.y);
    expect(box.y + box.height).toBeLessThanOrEqual(rail.y + rail.height + 0.5);
  }

  test("the pill's explanation fits the rail and still copies", async ({ page }) => {
    await open(page);
    await forceFallback(page);
    await setStatus(page, `error:${LONG_TOKEN}`);
    await pill(page).hover();
    const card = tooltipOf(pill(page));
    await expect(card).toBeVisible();
    expect(await card.evaluate((el) => getComputedStyle(el).position)).toBe("absolute");
    await insideRail(page, card);
    await card.click();
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
  });

  test("the marker's explanation fits the rail", async ({ page }) => {
    await open(page);
    await selectRoom(page, "Team Chat Room");
    await callRiverTest(page, "showRoomSyncError", "Room is full", `Room is at capacity ${LONG_TOKEN}`, "x");
    await forceFallback(page);
    const marker = page.getByTestId("room-sync-error-marker");
    await marker.hover();
    const card = tooltipOf(marker);
    await expect(card).toBeVisible();
    expect(await card.evaluate((el) => getComputedStyle(el).position)).toBe("absolute");
    await insideRail(page, card);
  });
});
