import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectListedRoom } from "./example-room";

// The owner's edit that the room would not take is refused before it is
// signed, with the reason shown under the field (freenet/river#703). A
// nickname limit over the room's fixed nickname ceiling (1000 bytes) is one.

const OWNED_ROOM = "Your Private Room";

async function openRoomDetails(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }
  await selectListedRoom(page, OWNED_ROOM);
  await page.getByTestId("room-title-button").click();
  await expect(page.getByTestId("edit-room-modal")).toBeVisible({ timeout: 5_000 });
}

function nicknameLimitInput(page: Page) {
  return page
    .getByTestId("edit-room-modal")
    .locator("div.mb-4", { hasText: "Max Nickname Size" })
    .locator("input");
}

test("a nickname limit over the ceiling is refused visibly", { tag: "@chromium-only" }, async ({
  page,
}) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomDetails(page);

  const input = nicknameLimitInput(page);
  const before = await input.inputValue();
  await input.fill("1001");
  await input.press("Tab");

  const error = page.getByTestId("edit-room-modal").getByTestId("config-error");
  await expect(error).toBeVisible();
  await expect(error).toContainText("at most 1000 bytes");

  // Back to the stored value: nothing to sign, and the refusal clears.
  await input.fill(before);
  await input.press("Tab");
  await expect(error).toHaveCount(0);
});

test("a refused rename keeps the dialog open with the reason", { tag: "@chromium-only" }, async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomDetails(page);

  // Over the room's max_room_name (100 bytes by default).
  const name = page.getByTestId("room-name-input");
  await name.fill("n".repeat(101));
  await name.press("Enter");

  await expect(page.getByTestId("edit-room-modal")).toBeVisible();
  await expect(page.getByTestId("edit-room-modal").getByTestId("config-error")).toBeVisible();
});
