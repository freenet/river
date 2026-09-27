import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

test.use({ viewport: { width: 1280, height: 800 } });

// (8, 8) sits in the outer p-4 padding, clear of the centred card.
async function clickOutsideCard(page: Page) {
  await page.mouse.click(8, 8);
}

test("clicking outside the Invite Member card dismisses it", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
  await page.getByTestId("invite-member-button").click();
  const modal = page.getByTestId("invite-member-modal");
  await expect(modal).toBeVisible();
  await modal.click();
  await expect(modal).toBeVisible();
  await clickOutsideCard(page);
  await expect(modal).toHaveCount(0);
});

test("clicking outside the Create Room card dismisses it", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await page.getByTestId("create-room-button").click();
  const modal = page.getByTestId("create-room-modal");
  await expect(modal).toBeVisible();
  await modal.click();
  await expect(modal).toBeVisible();
  await clickOutsideCard(page);
  await expect(modal).toHaveCount(0);
});
