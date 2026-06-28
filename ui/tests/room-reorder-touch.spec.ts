import { test, expect, Page } from "@playwright/test";

// Regression test for freenet/river#348.
//
// PR #346 added drag-and-drop room reordering using HTML5 `draggable` + drag
// events, which browsers do NOT emit for touch gestures (iOS Safari / Android
// Chrome). Touch users could therefore never reorder rooms. The fix adds a
// "reorder mode" toggle in the Rooms header that reveals per-row up/down
// controls. Those controls reorder via a plain `onclick`, which fires for both
// pointer AND touch input, so reordering now works regardless of input type.
//
// The controls call the same input-agnostic persistence helpers
// (`move_room_up` / `move_room_down` in ui/src/room_data.rs) that the drag path
// uses, so this test exercises the full gesture → reorder wiring.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// Stable per-room testids (`room-item-{base58(owner_vk)}`, added in #363) in
// rail (DOM) order. Identity-based, so it's robust to duplicate display names.
async function railOrder(page: Page): Promise<string[]> {
  return await page
    .locator('[data-testid^="room-item-"]')
    .evaluateAll((els) =>
      els.map((e) => e.getAttribute("data-testid") || "").filter(Boolean)
    );
}

test.describe("Touch-friendly room reorder (#348)", () => {
  // Force a desktop width so the rail is visible on the mobile Playwright
  // projects too. The up/down controls use a plain onclick (fires for touch and
  // pointer alike), so the reorder behaviour is identical across input types —
  // the bug was specifically that drag EVENTS never fire for touch.
  test.use({ viewport: { width: 1280, height: 800 } });

  test("reorder toggle reveals per-row up/down controls", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    // Example data ships three rooms, so reordering is meaningful.
    const names = await railOrder(page);
    expect(names.length).toBeGreaterThanOrEqual(2);

    // Controls are hidden until reorder mode is on.
    await expect(page.getByTestId("reorder-room-up")).toHaveCount(0);
    await expect(page.getByTestId("reorder-room-down")).toHaveCount(0);

    await page.getByTestId("reorder-rooms-toggle").click();

    // One up + one down control per row now.
    await expect(page.getByTestId("reorder-room-up")).toHaveCount(names.length);
    await expect(page.getByTestId("reorder-room-down")).toHaveCount(
      names.length
    );

    // Boundary controls are disabled: the first row can't move up, the last
    // row can't move down.
    await expect(page.getByTestId("reorder-room-up").first()).toBeDisabled();
    await expect(page.getByTestId("reorder-room-down").last()).toBeDisabled();

    // Toggling off hides the controls again.
    await page.getByTestId("reorder-rooms-toggle").click();
    await expect(page.getByTestId("reorder-room-up")).toHaveCount(0);
  });

  test("move-down swaps a room with the one below it", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const before = await railOrder(page);
    expect(before.length).toBeGreaterThanOrEqual(2);

    await page.getByTestId("reorder-rooms-toggle").click();
    await expect(page.getByTestId("reorder-room-down").first()).toBeEnabled();

    // Move the first room down one slot.
    await page.getByTestId("reorder-room-down").first().click();

    await expect(async () => {
      const after = await railOrder(page);
      expect(after[0]).toBe(before[1]);
      expect(after[1]).toBe(before[0]);
      // Every other row keeps its position.
      expect(after.slice(2)).toEqual(before.slice(2));
    }).toPass({ timeout: 5_000 });
  });

  test("move-up swaps a room with the one above it", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    const before = await railOrder(page);
    expect(before.length).toBeGreaterThanOrEqual(2);

    await page.getByTestId("reorder-rooms-toggle").click();

    // Move the second room up one slot (its up control is the 2nd up button).
    await page.getByTestId("reorder-room-up").nth(1).click();

    await expect(async () => {
      const after = await railOrder(page);
      expect(after[0]).toBe(before[1]);
      expect(after[1]).toBe(before[0]);
      expect(after.slice(2)).toEqual(before.slice(2));
    }).toPass({ timeout: 5_000 });
  });
});
