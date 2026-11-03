import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectListedRoom, selectRoom } from "./example-room";
import { callRiverTest } from "./river-test";
import { DM_PEER, OTHER_DM_PEER, DmPeer } from "./dm-thread";

// Archiving a DM thread from the rail (#266, follow-up to #261), and its
// Undo in the app toast (`components/toast.rs`).
//
// The threads are real: `appendDmsForPeer` seeds signed DMs from a test peer
// (`test_hooks.rs`). The archive cutoff and revival rules are pinned by the
// `filter_rail_entries_*` / `archive_cutoff_*` unit tests in
// `dm_rail_section.rs`; this checks what the reader sees.
//
// The timer tests run in real time. `page.clock` would stall the app, whose
// defers, spawns and sleeps are all `setTimeout`.

function railRow(page: Page, peer: DmPeer) {
  return page.locator(".dm-rail-row-btn", { hasText: peer.name });
}

function toast(page: Page) {
  return page.getByTestId("toast");
}

// Phone widths keep the DM rail in the rooms panel, behind the hamburger.
async function showRail(page: Page, peer: DmPeer) {
  const row = railRow(page, peer);
  const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
  if (!(await row.isVisible()) && (await hamburger.count()) > 0) await hamburger.click();
  await expect(row).toBeVisible({ timeout: 5_000 });
}

async function archive(page: Page, peer: DmPeer) {
  await showRail(page, peer);
  const row = railRow(page, peer);
  // The ✕ reveals on hover on a hover pointer; on touch it always shows.
  await row.hover();
  await row.locator("xpath=..").locator(".dm-archive-btn").click();
  await expect(row).toHaveCount(0);
}

async function seedThreads(page: Page, ...peers: DmPeer[]) {
  await page.goto("/");
  await waitForApp(page);
  // The hooks seed the threads in the open room.
  await selectRoom(page, "Team Chat Room");
  for (const peer of peers) await callRiverTest(page, "appendDmsForPeer", peer.index, 3);
}

test.describe("DM archive (#266)", () => {
  test("with no DMs the rail shows no section", { tag: "@chromium-only" }, async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.goto("/");
    await waitForApp(page);

    // An empty rail must not show a stray "Direct messages" header.
    await expect(page.getByRole("heading", { name: "Direct messages" })).toHaveCount(0);
    // The old in-modal "Hide" button stays gone (#266).
    await expect(page.getByRole("button", { name: "Hide" })).toHaveCount(0);
  });

  test("archiving offers Undo, which brings the thread back", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await seedThreads(page, DM_PEER);

    await archive(page, DM_PEER);
    await expect(toast(page)).toBeVisible();
    await expect(page.getByTestId("toast-message")).toHaveText(`Archived conversation with ${DM_PEER.name}`);

    await page.getByTestId("toast-action").click();
    await expect(toast(page)).toHaveCount(0);
    await expect(railRow(page, DM_PEER)).toBeVisible();
    // Un-archived, not merely revived: nothing is left in the Archived panel.
    await expect(page.getByRole("button", { name: /^Archived \(/ })).toHaveCount(0);
  });

  test("on a phone the toast stays up when the chat replaces the rooms panel", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 760 });
    await seedThreads(page, DM_PEER);

    await archive(page, DM_PEER);
    await expect(toast(page)).toBeVisible();

    // Opening a room shows the chat panel and hides the rail with the room
    // list. The rail used to own the toast, so it went with it.
    await selectListedRoom(page, "Your Private Room");
    await expect(page.getByTestId("room-list")).toBeHidden();
    await expect(toast(page)).toBeVisible();

    await page.getByTestId("toast-action").click();
    await expect(toast(page)).toHaveCount(0);
    await showRail(page, DM_PEER);
  });
});

test.describe("DM archive toast lifetime", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the toast goes by itself", async ({ page }) => {
    await seedThreads(page, DM_PEER);
    await archive(page, DM_PEER);
    await expect(toast(page)).toBeVisible();
    await expect(toast(page)).toBeHidden({ timeout: 8_000 });
    // Expiry is not Undo: the thread stays archived.
    await expect(railRow(page, DM_PEER)).toHaveCount(0);
  });

  test("an earlier toast's timer does not close a later one", async ({ page }) => {
    await seedThreads(page, DM_PEER, OTHER_DM_PEER);

    await archive(page, DM_PEER);
    const firstShown = Date.now();
    await page.waitForTimeout(3_000);
    await archive(page, OTHER_DM_PEER);
    const message = page.getByTestId("toast-message");
    await expect(message).toHaveText(`Archived conversation with ${OTHER_DM_PEER.name}`);

    // Past the first toast's 5s: its timer has fired, and must have closed
    // nothing, since the toast it was for is gone.
    await page.waitForTimeout(Math.max(0, firstShown + 6_000 - Date.now()));
    await expect(message).toHaveText(`Archived conversation with ${OTHER_DM_PEER.name}`);

    // The later toast's own timer still closes it.
    await expect(toast(page)).toBeHidden({ timeout: 5_000 });
  });
});
