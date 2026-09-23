import { expect, Page } from "@playwright/test";

// Steps shared by the message specs. Narrow-screen navigation stays in the
// spec that needs it: opening the hamburger, or widening to reach the list
// and restoring the viewport afterwards. So does anything the shell being
// visible does not prove (composer, reply strip, scroll position).

export async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

/**
 * Choose an example room from the room list. The list has to already be on
 * screen. Scoped there because the header title button carries the same
 * accessible name once a room is open.
 */
export async function selectListedRoom(page: Page, roomName: string) {
  const roomBtn = page.getByTestId("room-list").getByRole("button", { name: roomName });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: roomName })).toBeVisible({
    timeout: 5_000,
  });
}
