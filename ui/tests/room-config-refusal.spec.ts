import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectListedRoom } from "./example-room";

// The owner's edit that the room would not take, or that would drop member
// nicknames, is refused before it is signed, with the reason shown under the
// field (freenet/river#703).

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

test("lowering the nickname limit below held nicknames is refused visibly", { tag: "@chromium-only" }, async ({
  page,
}) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomDetails(page);

  const input = nicknameLimitInput(page);
  const before = await input.inputValue();
  await input.fill("1");
  await input.press("Tab");

  const error = page.getByTestId("edit-room-modal").getByTestId("config-error");
  await expect(error).toBeVisible();
  await expect(error).toContainText("longer than 1 bytes");

  // Back to the stored value: nothing to sign, and the refusal clears.
  await input.fill(before);
  await input.press("Tab");
  await expect(error).toHaveCount(0);
});
