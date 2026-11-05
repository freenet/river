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
const PILL_NAME = "WebSocket connection status";
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
      display: cs.display,
      visibility: cs.visibility,
      transition: cs.transitionDuration,
      animation: cs.animationName,
    };
  });
  expect(style).toEqual({
    display: "block",
    visibility: "visible",
    transition: "0s",
    animation: "none",
  });
}

async function withinViewport(page: Page, el: Locator) {
  const box = (await el.boundingBox())!;
  const vp = page.viewportSize()!;
  expect(box.x, "left edge on screen").toBeGreaterThanOrEqual(0);
  expect(box.x + box.width, "right edge on screen").toBeLessThanOrEqual(vp.width + 0.5);
  expect(box.y, "top edge on screen").toBeGreaterThanOrEqual(0);
  expect(box.y + box.height, "bottom edge on screen").toBeLessThanOrEqual(vp.height + 0.5);
}

/// Clicking a copy host's shown card copies what clicking the host did, and
/// says so again. The toast is replaced first, so its text proves this click.
async function expectCardCopies(page: Page, host: Locator, copied: string) {
  await callRiverTest(page, "showToast", "Before the card click", null);
  await expect(page.getByTestId("toast-message")).toHaveText("Before the card click");
  await host.hover();
  const card = tooltipOf(host);
  await expect(card).toBeVisible();
  await card.click();
  await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
  expect(await clipboardWrites(page)).toEqual([copied, copied]);
}

const ROOM_NOT_FOUND = "This room could not be found on the network — it may have been removed.";

/// Press a control from the keyboard, which keeps focus-visible for later focus.
async function pressFromKeyboard(page: Page, control: Locator) {
  await control.focus();
  await page.keyboard.press("Enter");
}

/// Example rooms load in no fixed order. Move Team Chat Room off the ends of
/// the rail with the reorder controls, so another row lies on each side of it
/// whichever way its card opens.
async function placeTeamChatInMiddle(page: Page) {
  const names = page.getByTestId("room-list").locator('[data-testid^="room-item-"] button .truncate');
  const indexOfTeamChat = async () => (await names.allTextContents()).indexOf("Team Chat Room");
  const count = await names.count();
  expect(count, "a row on each side needs three rooms").toBeGreaterThanOrEqual(3);
  const at = await indexOfTeamChat();
  if (at > 0 && at < count - 1) return;
  const item = page.getByTestId("room-list").locator('[data-testid^="room-item-"]', { hasText: "Team Chat Room" });
  await pressFromKeyboard(page, page.getByTestId("reorder-rooms-toggle"));
  await pressFromKeyboard(page, item.getByTestId(at === 0 ? "reorder-room-down" : "reorder-room-up"));
  await expect.poll(indexOfTeamChat).toBe(at === 0 ? 1 : count - 2);
  await pressFromKeyboard(page, page.getByTestId("reorder-rooms-toggle"));
  await expect(page.getByTestId("reorder-room-up")).toHaveCount(0);
}

/// Team Chat Room open with a sync error, its row focused from the keyboard
/// so the marker's card stays shown wherever the pointer goes. Hooks and keys
/// only: no pointer has been used on the page, so script focus is focus-visible.
async function focusFailedRoomRow(page: Page): Promise<{ row: Locator; card: Locator }> {
  await placeTeamChatInMiddle(page);
  await callRiverTest(page, "switchRoom", "Team Chat Room");
  await callRiverTest(page, "showRoomSyncError", "Room not found", ROOM_NOT_FOUND, "Initial sync failed 10 times");
  await expect(page.getByTestId("room-sync-error")).toBeVisible();
  const marker = page.getByTestId("room-sync-error-marker");
  const row = page.getByTestId("room-list").locator("button", { has: marker });
  await row.focus();
  expect(await row.evaluate((el) => el.matches(":focus-visible"))).toBe(true);
  const card = tooltipOf(marker);
  await expectShownAtOnce(card);
  return { row, card };
}

/// Another room's row under the card's painted box, and a point inside both,
/// clear of their edges. Whichever neighbour the engine placed it over.
async function roomUnderCard(page: Page, card: Locator) {
  const found = await card.evaluate((cardEl) => {
    const inset = 3;
    const box = cardEl.querySelector(".tooltip-box")!.getBoundingClientRect();
    let best: { testId: string; name: string; x: number; y: number; area: number } | null = null;
    for (const item of document.querySelectorAll('[data-testid="room-list"] [data-testid^="room-item-"]')) {
      if (item.contains(cardEl)) continue;
      const button = item.querySelector("button")!;
      const r = button.getBoundingClientRect();
      const left = Math.max(box.left, r.left) + inset;
      const right = Math.min(box.right, r.right) - inset;
      const top = Math.max(box.top, r.top) + inset;
      const bottom = Math.min(box.bottom, r.bottom) - inset;
      const area = (right - left) * (bottom - top);
      if (right > left && bottom > top && (best === null || area > best.area)) {
        best = {
          testId: item.getAttribute("data-testid")!,
          name: button.querySelector(".truncate")!.textContent!,
          x: (left + right) / 2,
          y: (top + bottom) / 2,
          area,
        };
      }
    }
    return best;
  });
  expect(found, "the card covers another room's row").not.toBeNull();
  return found!;
}

/// A real click on the shown card where it covers another room opens that
/// room: the explanation does not take the click, and copies nothing.
async function expectClickPassesThroughCard(page: Page, card: Locator) {
  const { testId, name, x, y } = await roomUnderCard(page, card);
  expect(name).not.toBe("Team Chat Room");
  const target = page.getByTestId(testId).locator("button").first();
  await page.mouse.move(x, y);
  expect(await card.isVisible(), "the card is still shown under the pointer").toBe(true);
  const reached = await target.evaluate(
    (button, [px, py]) => {
      const hit = document.elementFromPoint(px, py);
      return hit !== null && button.contains(hit);
    },
    [x, y]
  );
  expect(reached, `the point over ${name} reaches its row, not the card`).toBe(true);
  await page.mouse.click(x, y);
  await expect(page.getByRole("heading", { name, exact: true })).toBeVisible();
  await expect(page.getByTestId("room-sync-error")).toHaveCount(0);
  expect(await clipboardWrites(page)).toEqual([]);
}

test.describe("Connection error details", () => {
  test("a closed connection reads Disconnected, explains on hover and copies on click", async ({
    page,
  }) => {
    await open(page);
    // Offline example data starts Disconnected with no error to explain.
    await expect(pillLabel(page)).toHaveText("Disconnected");
    await expect(pill(page)).toHaveJSProperty("tagName", "DIV");
    // A generic div has no naming semantics of its own: check the attribute.
    await expect(pill(page)).toHaveAttribute("aria-label", `${PILL_NAME}: Disconnected`);
    await expect(tooltipOf(pill(page))).toHaveCount(0);

    await setStatus(page, "error");

    await expect(pillLabel(page)).toHaveText("Disconnected");
    // The button's name carries the status it shows, not just what it is.
    await expect(pill(page)).toHaveAccessibleName(`${PILL_NAME}: Disconnected`);
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

    // The shown card copies too: unlike an explain-only card, it takes clicks.
    await expectCardCopies(page, pill(page), copied);
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
    // Named for what it shows now, though the report it copies is the
    // earlier Disconnected one.
    await expect(pill(page)).toHaveAccessibleName(`${PILL_NAME}: Connecting...`);
    await pill(page).hover();
    await expect(tooltipOf(pill(page))).toContainText("Connection closed");
    await pill(page).click();
    await expect(page.getByTestId("toast-message")).toHaveText("Error details copied");
    expect((await clipboardWrites(page))[0]).toMatch(/^River: Disconnected\nConnection closed\n/);

    await setStatus(page, "disconnected");
    await expect(pillLabel(page)).toHaveText("Disconnected");
    await expect(pill(page)).toHaveAccessibleName(`${PILL_NAME}: Disconnected`);
    await expect(tooltipOf(pill(page))).toContainText("Connection closed");

    await setStatus(page, "connected");
    await expect(pillLabel(page)).toHaveText("Connected");
    await expect(pill(page)).toHaveJSProperty("tagName", "DIV");
    await expect(pill(page)).toHaveAttribute("aria-label", `${PILL_NAME}: Connected`);
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
    await expect(pill(page)).toHaveAccessibleName(`${PILL_NAME}: Refreshing...`);
    await expect(pill(page).getByTestId("connection-status-dot")).toHaveClass(/(^|\s)bg-warning(\s|$)/);
    await pill(page).hover();
    await expect(tooltipOf(pill(page))).toContainText("Authentication expired. Refreshing the page.");
  });

  test("an invalid operation is named by its label", async ({ page }) => {
    await open(page);
    await callRiverTest(
      page,
      "setSyncErrorReport",
      "Invalid operation",
      "The node refused the request.",
      "x"
    );
    await expect(pillLabel(page)).toHaveText("Invalid operation");
    await expect(pill(page)).toHaveRole("button");
    await expect(pill(page)).toHaveAccessibleName(`${PILL_NAME}: Invalid operation`);
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
    const [copied] = await clipboardWrites(page);
    expect(copied).toMatch(
      /^River: Room not found\nThis room could not be found on the network — it may have been removed\.\nInitial sync failed 10 times\nTime: /
    );

    await expectCardCopies(page, banner, copied);
  });

  /// The room row's button that holds the marker.
  function rowOf(page: Page, marker: Locator): Locator {
    return page.getByTestId("room-list").locator("button", { has: marker });
  }

  /// An explanation only: no copy hint, no button of its own, and nothing
  /// copied by showing it.
  async function expectExplainsOnly(page: Page, marker: Locator) {
    await expect(tooltipOf(marker)).toContainText("could not be found on the network");
    await expect(tooltipOf(marker)).not.toContainText("Click to copy details");
    await expect(marker.locator("button")).toHaveCount(0);
    expect(await clipboardWrites(page)).toEqual([]);
  }

  // A contended `try_read` neither returns the status nor subscribes, so the
  // banner must retry on its own; nothing else here writes a signal it reads.
  test("the banner recovers when its first read of the error fails", async ({ page }) => {
    await open(page);
    await selectRoom(page, "Team Chat Room");
    await nextFrames(page);
    expect(await callRiverTest(page, "takeBannerReadFailures")).toBe(0);
    await callRiverTest(page, "takeBannerErrorReads");

    await callRiverTest(
      page,
      "showRoomSyncErrorFailingBannerRead",
      "Room not found",
      "This room could not be found on the network — it may have been removed.",
      "Initial sync failed 10 times"
    );
    // The error was delivered (the rail reads it), and the banner's read of it failed.
    await expect(page.getByTestId("room-sync-error-marker")).toBeVisible();
    await expect.poll(() => callRiverTest(page, "takeBannerReadFailures")).toBe(1);

    // With no click, room switch or further status write, a later read finds it.
    const banner = page.getByTestId("room-sync-error");
    await expect(banner).toContainText("Room not found");
    expect(await callRiverTest(page, "takeBannerErrorReads")).toBeGreaterThanOrEqual(1);
    expect(await callRiverTest(page, "takeBannerReadFailures")).toBe(0);
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
    await expectExplainsOnly(page, marker);
    await withinViewport(page, tooltipOf(marker));

    // Anywhere on the row explains it, not only the icon. Leaving and coming
    // back also checks the card follows the pointer after it has been placed
    // (Gecko left a flipped card's visibility stale; see main.css).
    const row = rowOf(page, marker);
    await page.getByTestId("room-sync-error").hover();
    await expect(tooltipOf(marker)).toBeHidden();
    const rowBox = (await row.boundingBox())!;
    const markerBox = (await marker.boundingBox())!;
    const awayX = 8;
    expect(markerBox.x, "the hover point is clear of the marker").toBeGreaterThan(rowBox.x + awayX + 20);
    await row.hover({ position: { x: awayX, y: rowBox.height / 2 } });
    await expectShownAtOnce(tooltipOf(marker));
    await expectExplainsOnly(page, marker);
    await withinViewport(page, tooltipOf(marker));

    await callRiverTest(page, "switchRoom", "Public Discussion Room");
    await expect(page.getByTestId("room-sync-error")).toHaveCount(0);
    await marker.click();
    await expect(page.getByTestId("room-sync-error")).toBeVisible();
    expect(await clipboardWrites(page)).toEqual([]);
  });

  test("keyboard focus on the marker's row explains it, and Enter opens the room", async ({
    page,
  }) => {
    await open(page);
    // Through the hook, not a click: no pointer has been used on the page, so
    // script focus is focus-visible.
    await callRiverTest(page, "switchRoom", "Team Chat Room");
    await callRiverTest(
      page,
      "showRoomSyncError",
      "Room not found",
      "This room could not be found on the network — it may have been removed.",
      "Initial sync failed 10 times"
    );
    await expect(page.getByTestId("room-sync-error")).toBeVisible();
    await callRiverTest(page, "switchRoom", "Public Discussion Room");
    await expect(page.getByTestId("room-sync-error")).toHaveCount(0);

    const marker = page.getByTestId("room-sync-error-marker");
    await rowOf(page, marker).focus();
    await expectShownAtOnce(tooltipOf(marker));
    await expectExplainsOnly(page, marker);
    await withinViewport(page, tooltipOf(marker));

    await page.keyboard.press("Enter");
    await expect(page.getByTestId("room-sync-error")).toBeVisible();
    expect(await clipboardWrites(page)).toEqual([]);
  });

  test("a click on the marker's card opens the room beneath it", async ({ page }) => {
    await open(page);
    const { card } = await focusFailedRoomRow(page);
    // Anchored where the engine supports it, else the absolute fallback.
    const anchored = await page.evaluate(
      () => CSS.supports("anchor-name: --a") && CSS.supports("position-area: top")
    );
    expect(await card.evaluate((el) => getComputedStyle(el).position)).toBe(
      anchored ? "fixed" : "absolute"
    );
    await expectClickPassesThroughCard(page, card);
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

  test("a click on the marker's card opens the room beneath it", async ({ page }) => {
    await open(page);
    await forceFallback(page);
    const { card } = await focusFailedRoomRow(page);
    expect(await card.evaluate((el) => getComputedStyle(el).position)).toBe("absolute");
    await expectClickPassesThroughCard(page, card);
  });
});
