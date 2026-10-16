import { test, expect } from "@playwright/test";
import { waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// Regression coverage for freenet/river#802.
//
// Once one room has loaded, the rail shows the list rather than its
// "Migrating your rooms…" state, so while a delegate migration was still
// restoring rooms a partly restored list looked final. The rail now carries a
// small "Restoring more rooms…" note under the list until the migration is
// done. The example build is seeded with rooms and never migrates, so both
// sources are driven through hooks (test_hooks.rs, example-data + no-sync
// only): the sweep's `Migrating` with a load worker running
// (`setRoomsLoadStateKeepingRooms`, which leaves ROOMS alone), and a journaled
// walk (`setWalkRestoringRooms`).

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

  test("a Migrating state left behind by settled workers shows no note", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const rows = page.locator('[data-testid^="room-item-"]');
    const note = page.getByTestId("room-list-restoring-more");
    await expect(rows.first()).toBeVisible({ timeout: 10_000 });

    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "migrating");
    await expect(note).toBeVisible({ timeout: 5_000 });

    // The production shape: the last worker settles and nothing resolves
    // `Migrating` while rooms are present. Only the worker counter changes,
    // so the note going proves the rail re-renders on it.
    await callRiverTest(page, "releaseHeldLoadWorker");
    await expect(note).toHaveCount(0, { timeout: 5_000 });

    // A later worker starts with the state already `Migrating` (another
    // generation's probe): the note comes back on the worker alone.
    await callRiverTest(page, "setRoomsLoadStateKeepingRooms", "migrating");
    await expect(note).toBeVisible({ timeout: 5_000 });
    await callRiverTest(page, "releaseHeldLoadWorker");
    await expect(note).toHaveCount(0, { timeout: 5_000 });
  });

  test("shows while a journaled walk may still bring rooms, and goes when it ends", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const rows = page.locator('[data-testid^="room-item-"]');
    const note = page.getByTestId("room-list-restoring-more");
    await expect(rows.first()).toBeVisible({ timeout: 10_000 });
    await expect(note).toHaveCount(0);

    // Driven only through the journal session and its version counter: no
    // other signal the rail reads changes, so this also proves the rail
    // re-renders on the walk's start and end.
    await callRiverTest(page, "setWalkRestoringRooms", true);
    await expect(note).toBeVisible({ timeout: 5_000 });
    await callRiverTest(page, "setWalkRestoringRooms", false);
    await expect(note).toHaveCount(0, { timeout: 5_000 });
  });
});
