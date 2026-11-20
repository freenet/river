import { test, expect, Locator } from "@playwright/test";
import { selectRoom, waitForApp } from "./example-room";
import { callRiverTest } from "./river-test";

// Inspect the actual animation on the painted dot. Reading a stylesheet alone
// would miss a selector or reduced-motion override that didn't reach it.
async function animatedProperties(dot: Locator) {
  return dot.evaluate(el => el.getAnimations().flatMap(animation =>
    (animation.effect as KeyframeEffect).getKeyframes().flatMap(frame => Object.keys(frame))
  ));
}

test("initial room sync keeps the row name and explains the wait above the conversation", {
  tag: "@chromium-only",
}, async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  const row = page.getByTestId("room-list").getByRole("button", { name: "Team Chat Room", exact: true });
  await expect(row).toBeVisible();
  await callRiverTest(page, "showRoomInitialSync");

  const rowDots = row.getByTestId("room-sync-dots");
  await expect(rowDots).toBeVisible();
  await expect(rowDots).toHaveAttribute("aria-hidden", "true");
  await expect(rowDots.locator(":scope > span")).toHaveCount(5);
  await expect(page.getByText(/Syncing room state from the network/)).toBeVisible();
  await expect(page.getByTestId("room-sync-banner-dots")).toBeVisible();

  // The same placeholder then exhausts its retry budget.
  await callRiverTest(page, "showRoomSyncError", "Room not found", "Room not found", "NotFound", false);
  await expect(page.getByTestId("room-sync-error")).toBeVisible();
  await expect(page.getByTestId("room-sync-error-marker")).toBeVisible();
  await expect(page.getByTestId("room-sync-dots")).toHaveCount(0);
  await expect(page.getByTestId("room-sync-banner-dots")).toHaveCount(0);
});

test("room dots move normally and shimmer in place with reduced motion", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  await callRiverTest(page, "showRoomInitialSync");
  const small = page.getByTestId("room-sync-banner-dots");
  const smallDot = small.locator(":scope > span").first();
  await expect(smallDot).toBeVisible();
  expect(await animatedProperties(smallDot)).toContain("transform");
  await page.emulateMedia({ reducedMotion: "reduce" });
  await expect.poll(() => animatedProperties(smallDot)).not.toContain("transform");
  expect(await animatedProperties(smallDot)).toContain("opacity");

  await callRiverTest(page, "setRoomsLoadState", "loading");
  const wave = page.getByTestId("conversation-rooms-loading-dots");
  await expect(wave).toBeVisible();
  await expect(wave).toHaveAttribute("aria-hidden", "true");
  const waveDots = wave.locator(":scope > span");
  await expect(waveDots).toHaveCount(10);
  const waveDot = waveDots.first();
  expect(await animatedProperties(waveDot)).toContain("opacity");
  expect(await animatedProperties(waveDot)).not.toContain("transform");
  await page.emulateMedia({ reducedMotion: "no-preference" });
  await expect.poll(() => animatedProperties(waveDot)).toContain("transform");
});
