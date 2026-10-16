import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// Regression coverage for freenet/river#802.
//
// Once one room has loaded, the rail shows the list rather than its
// "Migrating your rooms…" state, so while a delegate migration was still
// restoring rooms a partly restored list looked final. The rail now carries a
// small "Restoring more rooms…" note under the list until the migration is
// done. The example build is seeded with rooms and never migrates, so the
// load state is driven through `__riverTest.setRoomsLoadStateKeepingRooms`
// (test_hooks.rs, example-data + no-sync only), which leaves ROOMS alone.

test.describe("Restoring more rooms note", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("shows under a non-empty list while migrating, and goes when done", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const rows = page.locator('[data-testid^="room-item-"]');
    const note = page.getByTestId("room-list-restoring-more");
    await expect(rows.first()).toBeVisible({ timeout: 10_000 });
    // PREMISE: a settled list carries no note.
    await expect(note).toHaveCount(0);
    const roomCount = await rows.count();

    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "migrating");
    await expect(note).toBeVisible({ timeout: 5_000 });
    await expect(note.getByText("Restoring more rooms…")).toBeVisible();
    // The list itself still shows, unchanged, and the full-size empty-list
    // state does not.
    await expect(rows).toHaveCount(roomCount);
    await expect(page.getByTestId("room-list-migrating")).toHaveCount(0);

    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "loaded");
    await expect(note).toHaveCount(0, { timeout: 5_000 });
    await expect(rows).toHaveCount(roomCount);
  });
});
